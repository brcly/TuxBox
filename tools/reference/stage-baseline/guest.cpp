// Create one compute pipeline per translated program and time each call, for
// the per-stage baseline (tools/stage_baseline.py).
//
//   stage-baseline.exe --adapter VVVV:DDDD PLAN RESULTS
//
// Each PLAN line names one request and up to two translations of it:
//
//   REQUEST FILE WAVE ROOT [FILE WAVE ROOT]
//
// WAVE is the translation's required wave size (0 for none). As the TuxBox
// provider does, the first translation is used unless this device cannot run
// its wave size, and then the second. ROOT lists the program's resource
// bindings, one "KIND SPACE FIRST COUNT" per line (COUNT 0 is unbounded), and
// becomes the root signature: the bindings in descriptor tables, with both
// heaps directly indexed. The device is vkd3d-proton's, so the call times
// vkd3d-proton's DXIL to SPIR-V conversion and the driver compile together;
// the pipeline-timing layer times the driver's part on its own.
//
// RESULTS gets "begin" before each call and "done" after it, flushed, so a
// crash or a hang names its program. Times are Unix nanoseconds.
#define _CRT_SECURE_NO_WARNINGS
#include <windows.h>
#include <d3d12.h>
#include <dxgi1_4.h>
#include <wrl/client.h>
#include <cstdio>
#include <cstring>
#include <fstream>
#include <iterator>
#include <sstream>
#include <stdexcept>
#include <string>
#include <vector>

using Microsoft::WRL::ComPtr;

static void check(HRESULT hr, const char *what) {
    if (FAILED(hr)) {
        char text[160];
        std::snprintf(text, sizeof(text), "%s failed: 0x%08lx", what, static_cast<unsigned long>(hr));
        throw std::runtime_error(text);
    }
}

static long long unix_ns() {
    FILETIME now;
    GetSystemTimePreciseAsFileTime(&now);
    const unsigned long long ticks = (static_cast<unsigned long long>(now.dwHighDateTime) << 32) | now.dwLowDateTime;
    return static_cast<long long>(ticks - 116444736000000000ULL) * 100;
}

static std::vector<char> read_file(const std::string &path) {
    std::ifstream input(path, std::ios::binary);
    std::vector<char> data{std::istreambuf_iterator<char>{input}, {}};
    if (data.empty()) throw std::runtime_error("missing " + path);
    return data;
}

struct Binding {
    D3D12_DESCRIPTOR_RANGE_TYPE type;
    UINT space, first, count;
};

static std::vector<Binding> read_root(const std::string &path) {
    std::ifstream input(path);
    if (!input) throw std::runtime_error("missing " + path);
    std::vector<Binding> bindings;
    std::string kind;
    Binding b{};
    while (input >> kind >> b.space >> b.first >> b.count) {
        if (kind == "cbv") b.type = D3D12_DESCRIPTOR_RANGE_TYPE_CBV;
        else if (kind == "srv") b.type = D3D12_DESCRIPTOR_RANGE_TYPE_SRV;
        else if (kind == "uav") b.type = D3D12_DESCRIPTOR_RANGE_TYPE_UAV;
        else if (kind == "sampler") b.type = D3D12_DESCRIPTOR_RANGE_TYPE_SAMPLER;
        else throw std::runtime_error("bad binding kind in " + path);
        bindings.push_back(b);
    }
    return bindings;
}

static ComPtr<ID3D12RootSignature> make_root(ID3D12Device *device, const std::vector<Binding> &bindings) {
    // Bounded ranges share one table per heap; an unbounded range must end its
    // table, so each gets a table of its own.
    std::vector<std::vector<D3D12_DESCRIPTOR_RANGE1>> tables(2);
    for (const auto &b : bindings) {
        const bool sampler = b.type == D3D12_DESCRIPTOR_RANGE_TYPE_SAMPLER;
        D3D12_DESCRIPTOR_RANGE1 range{};
        range.RangeType = b.type;
        range.NumDescriptors = b.count ? b.count : UINT_MAX;
        range.BaseShaderRegister = b.first;
        range.RegisterSpace = b.space;
        range.Flags = sampler ? D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE
                              : D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE | D3D12_DESCRIPTOR_RANGE_FLAG_DATA_VOLATILE;
        range.OffsetInDescriptorsFromTableStart = D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND;
        if (b.count) tables[sampler ? 1 : 0].push_back(range);
        else tables.push_back({range});
    }
    std::vector<D3D12_ROOT_PARAMETER1> parameters;
    for (const auto &table : tables) {
        if (table.empty()) continue;
        D3D12_ROOT_PARAMETER1 parameter{};
        parameter.ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
        parameter.DescriptorTable = {static_cast<UINT>(table.size()), table.data()};
        parameter.ShaderVisibility = D3D12_SHADER_VISIBILITY_ALL;
        parameters.push_back(parameter);
    }
    D3D12_VERSIONED_ROOT_SIGNATURE_DESC desc{};
    desc.Version = D3D_ROOT_SIGNATURE_VERSION_1_1;
    desc.Desc_1_1 = {static_cast<UINT>(parameters.size()), parameters.data(), 0, nullptr,
                     D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED |
                         D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED};
    ComPtr<ID3DBlob> blob, error;
    HRESULT hr = D3D12SerializeVersionedRootSignature(&desc, &blob, &error);
    if (FAILED(hr) && error)
        throw std::runtime_error(std::string("root signature: ") + static_cast<const char *>(error->GetBufferPointer()));
    check(hr, "D3D12SerializeVersionedRootSignature");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0, blob->GetBufferPointer(), blob->GetBufferSize(), IID_PPV_ARGS(&root)),
          "CreateRootSignature");
    return root;
}

static ComPtr<ID3D12Device> open_device(const std::string &wanted, std::string &found) {
    ComPtr<IDXGIFactory1> factory;
    check(CreateDXGIFactory1(IID_PPV_ARGS(&factory)), "CreateDXGIFactory1");
    std::string seen;
    for (UINT n = 0;; ++n) {
        ComPtr<IDXGIAdapter1> adapter;
        if (factory->EnumAdapters1(n, &adapter) == DXGI_ERROR_NOT_FOUND)
            throw std::runtime_error("no adapter " + wanted + "; found:" + seen);
        DXGI_ADAPTER_DESC1 desc{};
        check(adapter->GetDesc1(&desc), "GetDesc1");
        char pci[16];
        std::snprintf(pci, sizeof(pci), "%04x:%04x", desc.VendorId, desc.DeviceId);
        seen += std::string(" ") + pci;
        if (wanted != pci) continue;
        ComPtr<ID3D12Device> device;
        check(D3D12CreateDevice(adapter.Get(), D3D_FEATURE_LEVEL_11_0, IID_PPV_ARGS(&device)), "D3D12CreateDevice");
        found = pci;
        return device;
    }
}

// vkd3d-proton converts DXIL to SPIR-V on the calling thread, and its
// control-flow analysis recurses deeply on large translated programs: on the
// 1 MiB main thread the first program over 1 MiB of DXIL overflowed the stack
// and the process died. The TuxBox provider compiles on 64 MiB stacks for the
// same reason (runtime/d3d12/src/big_stack.rs), so this does too.
constexpr SIZE_T STACK_BYTES = 64u << 20;

struct Call {
    ID3D12Device *device;
    const D3D12_COMPUTE_PIPELINE_STATE_DESC *desc;
    ComPtr<ID3D12PipelineState> *pipeline;
    HRESULT result;
};

static DWORD WINAPI create_pipeline(void *argument) {
    auto *call = static_cast<Call *>(argument);
    call->result = call->device->CreateComputePipelineState(call->desc, IID_PPV_ARGS(call->pipeline->GetAddressOf()));
    return 0;
}

static HRESULT on_big_stack(Call &call) {
    HANDLE thread = CreateThread(nullptr, STACK_BYTES, create_pipeline, &call, STACK_SIZE_PARAM_IS_A_RESERVATION, nullptr);
    if (!thread) throw std::runtime_error("CreateThread failed");
    WaitForSingleObject(thread, INFINITE);
    CloseHandle(thread);
    return call.result;
}

struct Variant {
    std::string file;
    UINT wave;
    std::string root;
};

int main(int argc, char **argv) {
    if (argc != 5 || std::strcmp(argv[1], "--adapter")) {
        std::fputs("usage: stage-baseline.exe --adapter VVVV:DDDD PLAN RESULTS\n", stderr);
        return 2;
    }
    FILE *results = std::fopen(argv[4], "ab");
    if (!results) return 3;
    try {
        std::string pci;
        auto device = open_device(argv[2], pci);
        D3D12_FEATURE_DATA_D3D12_OPTIONS1 waves{};
        check(device->CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS1, &waves, sizeof(waves)), "wave sizes");
        std::fprintf(results, "device %s %u %u\n", pci.c_str(), waves.WaveLaneCountMin, waves.WaveLaneCountMax);
        std::fflush(results);
        std::ifstream plan(argv[3]);
        std::string line;
        while (std::getline(plan, line)) {
            std::istringstream fields(line);
            std::string request;
            std::vector<Variant> variants;
            Variant v;
            fields >> request;
            while (fields >> v.file >> v.wave >> v.root) variants.push_back(v);
            const Variant *chosen = nullptr;
            UINT index = 0;
            for (UINT i = 0; i < variants.size() && !chosen; ++i) {
                const UINT wave = variants[i].wave;
                if (!wave || (wave >= waves.WaveLaneCountMin && wave <= waves.WaveLaneCountMax)) {
                    chosen = &variants[i];
                    index = i;
                }
            }
            if (!chosen) {
                std::fprintf(results, "skipped %s wave\n", request.c_str());
                std::fflush(results);
                continue;
            }
            const auto code = read_file(chosen->file);
            ComPtr<ID3D12RootSignature> root;
            try {
                root = make_root(device.Get(), read_root(chosen->root));
            } catch (const std::exception &error) {
                std::fprintf(results, "skipped %s root %s\n", request.c_str(), error.what());
                std::fflush(results);
                continue;
            }
            D3D12_COMPUTE_PIPELINE_STATE_DESC desc{};
            desc.pRootSignature = root.Get();
            desc.CS = {code.data(), code.size()};
            std::fprintf(results, "begin %s %u %lld\n", request.c_str(), index, unix_ns());
            std::fflush(results);
            ComPtr<ID3D12PipelineState> pipeline;
            Call call{device.Get(), &desc, &pipeline, E_FAIL};
            const long long start = unix_ns();
            const HRESULT hr = on_big_stack(call);
            const long long end = unix_ns();
            std::fprintf(results, "done %s %u %lld %lld 0x%08lx %zu\n", request.c_str(), index, start, end,
                         static_cast<unsigned long>(hr), code.size());
            std::fflush(results);
        }
        std::fputs("finished\n", results);
    } catch (const std::exception &error) {
        std::fprintf(results, "error %s\n", error.what());
        std::fclose(results);
        return 1;
    }
    std::fclose(results);
    return 0;
}
