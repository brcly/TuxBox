#include "selftest.h"
#include "game.h"
#include "gpu.h"
#include "screen.h"
#include "../../runtime/d3d12/compute_fixture.h"
#include "../../runtime/d3d12/frame_stream.h"
extern "C" {
#include "../../runtime/xmem/xmem.h"
}
#include <array>
#include <cmath>
#include <cstdint>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <iterator>
#include <vector>
#include "course_shader.h"
#include "gpu_indirect_shader.h"
#include "xbox_shader.h"
#include "raster_shader.h"
#include "lane_spill_shader.h"
#include "ray_query_shader.h"
#include "reclaim_shader.h"
#include "wave_alloc_shader.h"
#include <algorithm>
#include <memory>

namespace {
using compute_fixture::buffer;
using frame_abi::check;
using frame_abi::ComPtr;
using frame_abi::method;

// The Scarlett release record with ID3D12Device11 selects the queue and
// command-list tables a Series X|S title built with a recent GDK uses.
constexpr std::array<UINT, 18> scarlett_profile{
    0x20004, 0, UINT_MAX, 0, 0, 0, UINT_MAX, UINT_MAX, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0};

// A queue held on a label is released by the provider after five seconds, so a
// scenario that needs that release must already count as failed.
constexpr DWORD deadline_ms = 2000;

// Sentinel written to the wave-alloc UAV so a shader that never stores is
// distinguishable from one that stored the expected value (0).
constexpr UINT kWaveAllocSentinel = 0xdeadbeefu;

// Xbox command-list and queue slots (GDK gen9/d3d12_xs.h).
constexpr size_t slot_copy_memory = 62, slot_write32 = 71, slot_write64 = 72, slot_wait64 = 74,
                 slot_dispatch_flagged = 80, slot_copy_texture = 83, slot_build = 119,
                 slot_emit_sizes = 120, slot_dispatch_mesh = 129, slot_sync_top = 131, slot_sync_bottom = 132;
constexpr size_t slot_compute_root = 33, device_create_compute = 62, device_create_stream = 86;
constexpr size_t slot_bind_heaps = 32, slot_compute_table = 35, queue_wait = 17;
constexpr size_t slot_graphics_root = 34, slot_graphics_table = 36, slot_targets = 50, slot_clear_target = 52;
constexpr size_t slot_execute_indirect = 61, slot_root_constants = 39, slot_root_srv = 43,
                 slot_root_uav = 45;
constexpr size_t queue_signal = 16, queue_copy_mappings = 31, queue_execute_frame = 35, device_place = 44, device_create_list = 73;

using CopyMemory_ = void (WINAPI*)(ID3D12GraphicsCommandList*, UINT64, UINT64, UINT64, UINT);
using Write32 = void (WINAPI*)(ID3D12GraphicsCommandList*, UINT64, UINT, UINT, UINT);
using Write64 = void (WINAPI*)(ID3D12GraphicsCommandList*, UINT64, UINT64, UINT, UINT);
using Wait64 = void (WINAPI*)(ID3D12GraphicsCommandList*, UINT64, D3D12_COMPARISON_FUNC, UINT64, UINT);
using DispatchFlagged = void (WINAPI*)(ID3D12GraphicsCommandList*, UINT, UINT, UINT, UINT);
using DispatchMesh = void (WINAPI*)(ID3D12GraphicsCommandList*, UINT, UINT, UINT);
using Build = void (WINAPI*)(ID3D12GraphicsCommandList*, const void*, UINT, const void*);
using Sync = void (WINAPI*)(ID3D12GraphicsCommandList*, UINT, UINT, UINT64, UINT64);
using SignalQueue = HRESULT (WINAPI*)(ID3D12CommandQueue*, ID3D12Fence*, UINT64);
using ExecuteFrame = void (WINAPI*)(ID3D12CommandQueue*, UINT, ID3D12CommandList* const*, UINT64, UINT);
using CreateComputePipeline = HRESULT (WINAPI*)(ID3D12Device*, const D3D12_COMPUTE_PIPELINE_STATE_DESC*,
                                                UINT, const void*, REFIID, void**);
using CreateStreamPipeline = HRESULT (WINAPI*)(ID3D12Device*, const D3D12_PIPELINE_STATE_STREAM_DESC*,
                                               REFIID, void**);
using SetRoot = void (WINAPI*)(ID3D12GraphicsCommandList*, ID3D12RootSignature*);
using ExecuteIndirect_ = void (WINAPI*)(ID3D12GraphicsCommandList*, ID3D12CommandSignature*, UINT,
                                         ID3D12Resource*, UINT64, ID3D12Resource*, UINT64);
using SetConstants = void (WINAPI*)(ID3D12GraphicsCommandList*, UINT, UINT, const void*, UINT);
using SetUav = void (WINAPI*)(ID3D12GraphicsCommandList*, UINT, D3D12_GPU_VIRTUAL_ADDRESS);
using BindHeaps = void (WINAPI*)(ID3D12GraphicsCommandList*, UINT, ID3D12DescriptorHeap* const*);
using SetTable = void (WINAPI*)(ID3D12GraphicsCommandList*, UINT, D3D12_GPU_DESCRIPTOR_HANDLE);
using SetTargets = void (WINAPI*)(ID3D12GraphicsCommandList*, UINT, const D3D12_CPU_DESCRIPTOR_HANDLE*, BOOL,
                                  const D3D12_CPU_DESCRIPTOR_HANDLE*);
using WaitQueue = HRESULT (WINAPI*)(ID3D12CommandQueue*, ID3D12Fence*, UINT64);
using GpuHandle = UINT64 (WINAPI*)(ID3D12DescriptorHeap*);
using EmitSizes = void (WINAPI*)(ID3D12GraphicsCommandList*,
                                 const D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC*, UINT,
                                 const D3D12_GPU_VIRTUAL_ADDRESS*);
using Place = HRESULT (WINAPI*)(ID3D12Device*, UINT64, const D3D12_RESOURCE_DESC*, D3D12_RESOURCE_STATES,
                                const D3D12_CLEAR_VALUE*, REFIID, void**);

// Committed title memory. A Series title addresses the GPU with these pointers.
struct Memory {
    BYTE* base;
    explicit Memory(size_t bytes)
        : base(static_cast<BYTE*>(VirtualAlloc(nullptr, bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE))) {
        if (!base) throw std::runtime_error("allocate title memory");
    }
    ~Memory() { VirtualFree(base, 0, MEM_RELEASE); }
    Memory(const Memory&) = delete;
    Memory& operator=(const Memory&) = delete;
    UINT64 address(size_t offset = 0) const { return reinterpret_cast<UINT64>(base + offset); }
    UINT64 read64(size_t offset) const { return *reinterpret_cast<volatile const UINT64*>(base + offset); }
};

ComPtr<ID3D12GraphicsCommandList> open_list(ID3D12Device* device, D3D12_COMMAND_LIST_TYPE type,
                                             ID3D12CommandAllocator* allocator) {
    const UINT record[] = {UINT(type), 0, 1, 0};
    ComPtr<ID3D12GraphicsCommandList> list;
    check(method<frame_abi::CreateList>(device, device_create_list)(
              device, record, allocator, nullptr, IID_PPV_ARGS(&list)), "create list");
    check(list->Reset(allocator, nullptr), "open list");
    return list;
}

struct Queue {
    ComPtr<ID3D12CommandQueue> queue;
    ComPtr<ID3D12CommandAllocator> allocator;
    ComPtr<ID3D12GraphicsCommandList> list;
    ComPtr<ID3D12Fence> fence;
    UINT64 submitted = 0;

    Queue(ID3D12Device* device, D3D12_COMMAND_LIST_TYPE type) {
        D3D12_COMMAND_QUEUE_DESC desc{};
        desc.Type = type;
        check(device->CreateCommandQueue(&desc, IID_PPV_ARGS(&queue)), "create queue");
        check(device->CreateCommandAllocator(type, IID_PPV_ARGS(&allocator)), "create allocator");
        list = open_list(device, type, allocator.Get());
        check(device->CreateFence(0, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&fence)), "create fence");
    }
    void write32(UINT64 address, UINT value, UINT flush = 0) {
        method<Write32>(list.Get(), slot_write32)(list.Get(), address, value, flush, 0);
    }
    void write64(UINT64 address, UINT64 value, UINT flush = 0) {
        method<Write64>(list.Get(), slot_write64)(list.Get(), address, value, flush, 0);
    }
    void wait64(UINT64 address, UINT64 value) {
        method<Wait64>(list.Get(), slot_wait64)(list.Get(), address, D3D12_COMPARISON_FUNC_GREATER_EQUAL, value, 0);
    }
    void close() { check(list->Close(), "close list"); }
    // Reuse the allocator and list once everything submitted has finished.
    void reopen() {
        check(allocator->Reset(), "reset allocator");
        check(list->Reset(allocator.Get(), nullptr), "reopen list");
    }
    void signal() {
        check(method<SignalQueue>(queue.Get(), queue_signal)(queue.Get(), fence.Get(), ++submitted), "signal queue");
    }
    void submit() {
        close();
        ID3D12CommandList* batch[]{list.Get()};
        queue->ExecuteCommandLists(1, batch);
        signal();
    }
    // Wait until everything submitted has finished, or fail.
    void finish(const char* what) {
        HANDLE event = CreateEventW(nullptr, FALSE, FALSE, nullptr);
        if (!event) throw std::runtime_error("create completion event");
        const HRESULT armed = fence->SetEventOnCompletion(submitted, event);
        const DWORD waited = SUCCEEDED(armed) ? WaitForSingleObject(event, deadline_ms) : WAIT_FAILED;
        CloseHandle(event);
        check(armed, "arm completion event");
        if (waited != WAIT_OBJECT_0)
            throw std::runtime_error(std::string(what) + " did not finish within 2 seconds");
    }
};

void expect_value(const Memory& memory, size_t offset, UINT64 value, const char* what) {
    const UINT64 found = memory.read64(offset);
    if (found != value) {
        char message[160];
        std::snprintf(message, sizeof(message), "%s: expected %llu, found %llu", what,
                      static_cast<unsigned long long>(value), static_cast<unsigned long long>(found));
        throw std::runtime_error(message);
    }
}

// Both bottom-of-pipe writes store ordinary values, with or without a flush.
void labels(ID3D12Device* device) {
    Memory memory(65536);
    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    direct.write32(memory.address(0), 0x12345678);
    direct.write32(memory.address(8), 7, 1);
    direct.write64(memory.address(16), 0x1122334455667788ull);
    direct.write64(memory.address(24), 42, 0x100);
    direct.submit();
    direct.finish("label writes");
    expect_value(memory, 0, 0x12345678, "32-bit write");
    expect_value(memory, 8, 7, "32-bit write with a flush");
    expect_value(memory, 16, 0x1122334455667788ull, "64-bit write");
    expect_value(memory, 24, 42, "64-bit write with a flush");
}

// A compute queue waits for a label the direct queue writes later, and the
// direct queue then waits for the compute queue's answer.
void cross_queue(ID3D12Device* device) {
    constexpr size_t request = 0, reply = 8, payload = 65536, readback = 2 * 65536;
    constexpr UINT64 expected = 0x123456789abcdef0ull;
    Memory memory(3 * 65536);
    Queue compute(device, D3D12_COMMAND_LIST_TYPE_COMPUTE);
    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);

    compute.wait64(memory.address(request), 5);
    compute.write64(memory.address(payload), expected);
    compute.write64(memory.address(reply), 6);
    compute.submit();

    // Record the return wait while the compute queue is still waiting for the request.
    direct.wait64(memory.address(reply), 6);
    method<CopyMemory_>(direct.list.Get(), slot_copy_memory)(
        direct.list.Get(), memory.address(readback), memory.address(payload), sizeof(expected), 0);

    ComPtr<ID3D12CommandAllocator> request_allocator;
    check(device->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, IID_PPV_ARGS(&request_allocator)),
          "create request allocator");
    auto request_list = open_list(device, D3D12_COMMAND_LIST_TYPE_DIRECT, request_allocator.Get());
    method<Write64>(request_list.Get(), slot_write64)(request_list.Get(), memory.address(request), 5, 0, 0);
    check(request_list->Close(), "close request list");
    ID3D12CommandList* batch[]{request_list.Get()};
    direct.queue->ExecuteCommandLists(1, batch);
    direct.submit();

    try {
        compute.finish("compute queue waiting on a direct queue label");
        direct.finish("direct queue waiting on the compute reply");
    } catch (const std::runtime_error&) {
        // Let pending GPU work drain before its lists and title memory are released.
        InterlockedExchange64(reinterpret_cast<volatile LONG64*>(memory.base + request), 5);
        InterlockedExchange64(reinterpret_cast<volatile LONG64*>(memory.base + reply), 6);
        compute.finish("drain compute queue after failed wait");
        direct.finish("drain direct queue after failed wait");
        throw;
    }
    expect_value(memory, reply, 6, "compute reply");
    expect_value(memory, readback, expected, "payload copied after the compute reply");
}

// The compute queue submits through ExecuteCommandListsX as well.
void compute_frame(ID3D12Device* device) {
    Memory memory(65536);
    Queue compute(device, D3D12_COMMAND_LIST_TYPE_COMPUTE);
    compute.write64(memory.address(0), 99);
    compute.close();
    ID3D12CommandList* batch[]{compute.list.Get()};
    method<ExecuteFrame>(compute.queue.Get(), queue_execute_frame)(compute.queue.Get(), 1, batch, 1, 3);
    compute.signal();
    compute.finish("compute frame submission");
    expect_value(memory, 0, 99, "compute frame write");
}

// A command that cannot be recorded is left out on its own. The list still
// closes and its fence write, which another queue may need, still happens.
void skipped_command(ID3D12Device* device) {
    Memory memory(65536);
    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    auto sync_top = method<Sync>(direct.list.Get(), slot_sync_top);
    auto sync_bottom = method<Sync>(direct.list.Get(), slot_sync_bottom);
    sync_top(direct.list.Get(), 0xc280, 0x20000, 0, 0xffffffffff00ull);
    // The BVH builder passes no description for a build it cannot express.
    method<Build>(direct.list.Get(), slot_build)(direct.list.Get(), nullptr, 0, nullptr);
    sync_bottom(direct.list.Get(), 0xc280, 0x20000, 0, 0xffffffffff00ull);
    direct.write64(memory.address(0), 11);
    direct.submit();
    direct.finish("list with a skipped command");
    expect_value(memory, 0, 11, "write after a skipped command");
}

// CopyMemoryToMemoryX copies from the source to the destination.
void copy_memory(ID3D12Device* device) {
    constexpr size_t bytes = 4096, source = 0, destination = 65536;
    Memory memory(2 * 65536);
    for (size_t i = 0; i < bytes; ++i) {
        memory.base[source + i] = static_cast<BYTE>(i * 31 + 7);
        memory.base[destination + i] = 0xee;
    }
    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    method<CopyMemory_>(direct.list.Get(), slot_copy_memory)(
        direct.list.Get(), memory.address(destination), memory.address(source), bytes, 0);
    direct.submit();
    direct.finish("memory copy");
    if (std::memcmp(memory.base + source, memory.base + destination, bytes))
        throw std::runtime_error("memory copy did not reach the destination");
    if (memory.base[source + 1] != 38)
        throw std::runtime_error("memory copy overwrote its source");
}

// Compare a direct queue waiting before and immediately after a buffer copy.
// The compute producer remains unsubmitted during the observation window.
void direct_copy_wait(ID3D12Device* device) {
    constexpr UINT marker = 0x5a17c0de;
    constexpr UINT64 expected = 0x12345678;
    constexpr size_t label = 0, payload = 65536, copied = 2 * 65536;
    bool passed = true;
    for (bool after_copy : {false, true}) {
        Memory memory(3 * 65536);
        std::memset(memory.base + copied, 0xcc, sizeof(expected));
        Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
        Queue compute(device, D3D12_COMMAND_LIST_TYPE_COMPUTE);
        const UINT seeds[]{0, marker};
        auto upload = buffer(device, D3D12_HEAP_TYPE_UPLOAD, sizeof(seeds), D3D12_RESOURCE_STATE_GENERIC_READ);
        auto progress = buffer(device, D3D12_HEAP_TYPE_READBACK, sizeof(marker), D3D12_RESOURCE_STATE_COPY_DEST);
        compute_fixture::upload(upload.Get(), seeds, sizeof(seeds));
        auto* list = direct.list.Get();
        list->CopyBufferRegion(progress.Get(), 0, upload.Get(), 0, sizeof(marker));
        direct.submit();
        direct.finish("initialize prefix marker");
        direct.reopen();

        auto read_marker = [&] {
            D3D12_RANGE read{0, sizeof(marker)}, empty{};
            void* mapped{};
            check(progress->Map(0, &read, &mapped), "map prefix marker");
            const UINT value = *static_cast<const volatile UINT*>(mapped);
            progress->Unmap(0, &empty);
            return value;
        };
        if (read_marker() != 0) throw std::runtime_error("bad initial prefix marker");

        compute.write64(memory.address(payload), expected);
        compute.write64(memory.address(label), 1);
        if (!after_copy) direct.wait64(memory.address(label), 1);
        list->CopyBufferRegion(progress.Get(), 0, upload.Get(), sizeof(UINT), sizeof(marker));
        if (after_copy) direct.wait64(memory.address(label), 1);
        method<CopyMemory_>(list, slot_copy_memory)(list, memory.address(copied), memory.address(payload),
                                                   sizeof(expected), 0);
        direct.submit();

        Sleep(250);
        const UINT64 completed = direct.fence->GetCompletedValue();
        const UINT prefix = read_marker();
        const UINT64 copied_before = memory.read64(copied);
        compute.submit();
        compute.finish("direct-copy-wait producer");
        direct.finish("direct-copy-wait consumer");
        if (completed == UINT64_MAX) throw std::runtime_error("device removed during direct-copy-wait");
        expect_value(memory, label, 1, "producer label");
        expect_value(memory, payload, expected, "producer payload");
        if (read_marker() != marker) throw std::runtime_error("first buffer copy missing");
        const bool early = completed >= direct.submitted;
        const UINT64 value = memory.read64(copied);
        std::fprintf(stderr, "direct-copy-wait position=%s prefix_before=0x%08x copied_before=0x%016llx completed_before_producer=%s cpu=0x%08llx\n",
                     after_copy ? "after-copy" : "list-start", prefix, static_cast<unsigned long long>(copied_before), early ? "yes" : "no",
                     static_cast<unsigned long long>(value));
        passed &= prefix == (after_copy ? marker : 0u) && copied_before == 0xccccccccccccccccull && !early && value == expected;
    }
    if (!passed) throw std::runtime_error("direct queue copy ordering or readback mismatch");
}

// The compute queue waits for a label, then reads a guest texture the direct
// queue writes before setting that label. A provider that makes the writer wait
// for the earlier submitted reader deadlocks the two queues.
void shared_texture(ID3D12Device* device) {
    constexpr UINT width = 256, height = 256, pitch = 256;
    constexpr UINT64 bytes = UINT64(pitch) * height;
    Memory memory(4 * 65536);
    Memory label(65536);
    D3D12_RESOURCE_DESC desc{};
    desc.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    desc.Width = width;
    desc.Height = height;
    desc.DepthOrArraySize = 1;
    desc.MipLevels = 1;
    desc.SampleDesc.Count = 1;
    desc.Format = DXGI_FORMAT_R8_UNORM;
    desc.Layout = static_cast<D3D12_TEXTURE_LAYOUT>(0x105);
    ComPtr<ID3D12Resource> texture;
    check(method<Place>(device, device_place)(device, memory.address(), &desc, D3D12_RESOURCE_STATE_COMMON,
                                              nullptr, IID_PPV_ARGS(&texture)), "place guest texture");
    auto upload = buffer(device, D3D12_HEAP_TYPE_UPLOAD, bytes, D3D12_RESOURCE_STATE_GENERIC_READ);
    auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, bytes, D3D12_RESOURCE_STATE_COPY_DEST);
    std::vector<BYTE> expected(bytes);
    for (size_t i = 0; i < bytes; ++i) expected[i] = static_cast<BYTE>(i * 13 + i / pitch);
    compute_fixture::upload(upload.Get(), expected.data(), bytes);

    D3D12_TEXTURE_COPY_LOCATION image{}, linear{};
    image.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
    image.pResource = texture.Get();
    linear.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
    linear.PlacedFootprint.Footprint = {DXGI_FORMAT_R8_UNORM, width, height, 1, pitch};

    Queue compute(device, D3D12_COMMAND_LIST_TYPE_COMPUTE);
    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    compute.wait64(label.address(), 1);
    linear.pResource = readback.Get();
    method<frame_abi::Copy>(compute.list.Get(), slot_copy_texture)(
        compute.list.Get(), &linear, 0, 0, 0, &image, nullptr, 0);
    compute.submit();
    linear.pResource = upload.Get();
    method<frame_abi::Copy>(direct.list.Get(), slot_copy_texture)(
        direct.list.Get(), &image, 0, 0, 0, &linear, nullptr, 0);
    direct.write64(label.address(), 1);
    direct.submit();
    direct.finish("direct queue writing a texture a waiting queue reads");
    compute.finish("compute queue reading the texture");

    D3D12_RANGE range{0, static_cast<SIZE_T>(bytes)}, empty{};
    void* mapped{};
    check(readback->Map(0, &range, &mapped), "map readback");
    const bool matched = std::memcmp(mapped, expected.data(), bytes) == 0;
    readback->Unmap(0, &empty);
    if (!matched) throw std::runtime_error("the reader did not see the writer's texels");
}

// A console shader reaches the GPU through the translation cache. The provider
// substitutes an empty pipeline when no translation exists, so `verify` also
// checks that it reports loading the translated one.
void xbox_shader_scenario(ID3D12Device* device) {
    ComPtr<ID3D12RootSignature> root;
    D3D12_ROOT_SIGNATURE_DESC signature{};
    ComPtr<ID3DBlob> serialized;
    check(D3D12SerializeRootSignature(&signature, D3D_ROOT_SIGNATURE_VERSION_1, &serialized, nullptr),
          "serialize empty root signature");
    check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(),
                                      IID_PPV_ARGS(&root)), "create empty root signature");
    D3D12_COMPUTE_PIPELINE_STATE_DESC shader{};
    shader.pRootSignature = root.Get();
    shader.CS = {xbox_shader, sizeof(xbox_shader)};
    ComPtr<ID3D12PipelineState> pipeline;
    check(method<CreateComputePipeline>(device, device_create_compute)(device, &shader, 0, nullptr,
                                                                       IID_PPV_ARGS(&pipeline)),
          "create pipeline from a console shader");
    Memory memory(65536);
    Queue compute(device, D3D12_COMMAND_LIST_TYPE_COMPUTE);
    method<SetRoot>(compute.list.Get(), slot_compute_root)(compute.list.Get(), root.Get());
    compute.list->SetPipelineState(pipeline.Get());
    compute.list->Dispatch(1, 1, 1);
    compute.write64(memory.address(0), 3);
    compute.submit();
    compute.finish("dispatch of the console shader");
    expect_value(memory, 0, 3, "write after the dispatch");
}

// Dispatch a console wave fixture with a single raw UAV and check each word.
template <size_t Words>
void check_wave_shader(ID3D12Device* device, D3D12_SHADER_BYTECODE bytecode, const UINT (&expected)[Words]) {
    constexpr UINT words = static_cast<UINT>(Words);
    constexpr UINT64 bytes = words * sizeof(UINT);
    D3D12_DESCRIPTOR_RANGE range{D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 1, 0, 0, 0};
    D3D12_ROOT_PARAMETER table{};
    table.ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
    table.DescriptorTable = {1, &range};
    D3D12_ROOT_SIGNATURE_DESC signature{1, &table, 0, nullptr, D3D12_ROOT_SIGNATURE_FLAG_NONE};
    ComPtr<ID3DBlob> serialized;
    check(D3D12SerializeRootSignature(&signature, D3D_ROOT_SIGNATURE_VERSION_1, &serialized, nullptr),
          "serialize wave root signature");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(),
                                      IID_PPV_ARGS(&root)), "create wave root signature");
    D3D12_COMPUTE_PIPELINE_STATE_DESC shader{};
    shader.pRootSignature = root.Get();
    shader.CS = bytecode;
    ComPtr<ID3D12PipelineState> pipeline;
    check(method<CreateComputePipeline>(device, device_create_compute)(device, &shader, 0, nullptr,
                                                                       IID_PPV_ARGS(&pipeline)),
          "create pipeline from a console wave shader");

    D3D12_DESCRIPTOR_HEAP_DESC heap_desc{
        D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 1, D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    ComPtr<ID3D12DescriptorHeap> heap;
    check(device->CreateDescriptorHeap(&heap_desc, IID_PPV_ARGS(&heap)), "create wave view heap");
    const D3D12_CPU_DESCRIPTOR_HANDLE cpu{method<frame_abi::CpuHandle>(heap.Get(), 9)(heap.Get())};
    const D3D12_GPU_DESCRIPTOR_HANDLE gpu{method<GpuHandle>(heap.Get(), 10)(heap.Get())};

    auto seed = buffer(device, D3D12_HEAP_TYPE_UPLOAD, bytes, D3D12_RESOURCE_STATE_GENERIC_READ);
    auto output = buffer(device, D3D12_HEAP_TYPE_DEFAULT, bytes, D3D12_RESOURCE_STATE_COPY_DEST,
                         D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
    auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, bytes, D3D12_RESOURCE_STATE_COPY_DEST);
    const std::vector<UINT> unwritten(words, 0xdeadbeefu);
    compute_fixture::upload(seed.Get(), unwritten.data(), size_t(bytes));
    D3D12_UNORDERED_ACCESS_VIEW_DESC view{};
    view.ViewDimension = D3D12_UAV_DIMENSION_BUFFER;
    view.Format = DXGI_FORMAT_R32_TYPELESS;
    view.Buffer.NumElements = words;
    view.Buffer.Flags = D3D12_BUFFER_UAV_FLAG_RAW;
    device->CreateUnorderedAccessView(output.Get(), nullptr, &view, cpu);

    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    ID3D12GraphicsCommandList* list = direct.list.Get();
    list->CopyBufferRegion(output.Get(), 0, seed.Get(), 0, bytes);
    compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_COPY_DEST,
                                D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    ID3D12DescriptorHeap* heaps[]{heap.Get()};
    method<BindHeaps>(list, slot_bind_heaps)(list, 1, heaps);
    method<SetRoot>(list, slot_compute_root)(list, root.Get());
    list->SetPipelineState(pipeline.Get());
    method<SetTable>(list, slot_compute_table)(list, 0, gpu);
    list->Dispatch(1, 1, 1);
    compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                D3D12_RESOURCE_STATE_COPY_SOURCE);
    list->CopyBufferRegion(readback.Get(), 0, output.Get(), 0, bytes);
    direct.submit();
    direct.finish("dispatch of the console wave shader");

    D3D12_RANGE read{0, SIZE_T(bytes)}, empty{};
    void* mapped = nullptr;
    check(readback->Map(0, &read, &mapped), "map wave readback");
    std::vector<UINT> got(words);
    std::memcpy(got.data(), mapped, size_t(bytes));
    readback->Unmap(0, &empty);
    for (UINT i = 0; i < words; ++i) {
        if (got[i] != expected[i]) {
            char message[128];
            std::snprintf(message, sizeof(message), "wave result mismatch at thread %u: expected %u, found %u",
                          i, expected[i], got[i]);
            throw std::runtime_error(message);
        }
    }
}

// A console program that sums across its 64-lane waves. A host without such
// waves needs the translation made for 32 lanes.
void wave64_shader_scenario(ID3D12Device* device) {
    check_wave_shader(device, {wave_shader, sizeof(wave_shader)}, wave_shader_expected);
}

// A console program that stores its group ID, dispatched with more groups in
// a dimension than a desktop dispatch allows: 70,000 across, then 2 across
// and 70,000 down. Each record holds the ID of the group that wrote it.
void large_dispatch(ID3D12Device* device) {
    constexpr UINT across = 2, down = 70000, records = across * down, words = records * 4;
    constexpr UINT64 bytes = UINT64(words) * sizeof(UINT);
    D3D12_DESCRIPTOR_RANGE range{D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 1, 0, 0, 0};
    D3D12_ROOT_PARAMETER table{};
    table.ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
    table.DescriptorTable = {1, &range};
    D3D12_ROOT_SIGNATURE_DESC signature{1, &table, 0, nullptr, D3D12_ROOT_SIGNATURE_FLAG_NONE};
    ComPtr<ID3DBlob> serialized;
    check(D3D12SerializeRootSignature(&signature, D3D_ROOT_SIGNATURE_VERSION_1, &serialized, nullptr),
          "serialize group ID root signature");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(),
                                      IID_PPV_ARGS(&root)), "create group ID root signature");
    D3D12_COMPUTE_PIPELINE_STATE_DESC shader{};
    shader.pRootSignature = root.Get();
    shader.CS = {group_shader, sizeof(group_shader)};
    ComPtr<ID3D12PipelineState> pipeline;
    check(method<CreateComputePipeline>(device, device_create_compute)(device, &shader, 0, nullptr,
                                                                       IID_PPV_ARGS(&pipeline)),
          "create pipeline from the group ID program");

    D3D12_DESCRIPTOR_HEAP_DESC heap_desc{
        D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 1, D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    ComPtr<ID3D12DescriptorHeap> heap;
    check(device->CreateDescriptorHeap(&heap_desc, IID_PPV_ARGS(&heap)), "create group ID view heap");
    const D3D12_CPU_DESCRIPTOR_HANDLE cpu{method<frame_abi::CpuHandle>(heap.Get(), 9)(heap.Get())};
    const D3D12_GPU_DESCRIPTOR_HANDLE gpu{method<GpuHandle>(heap.Get(), 10)(heap.Get())};
    auto seed = buffer(device, D3D12_HEAP_TYPE_UPLOAD, bytes, D3D12_RESOURCE_STATE_GENERIC_READ);
    auto output = buffer(device, D3D12_HEAP_TYPE_DEFAULT, bytes, D3D12_RESOURCE_STATE_COPY_DEST,
                         D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
    auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, bytes, D3D12_RESOURCE_STATE_COPY_DEST);
    const std::vector<UINT> unwritten(words, 0xdeadbeefu);
    compute_fixture::upload(seed.Get(), unwritten.data(), size_t(bytes));
    D3D12_UNORDERED_ACCESS_VIEW_DESC view{};
    view.ViewDimension = D3D12_UAV_DIMENSION_BUFFER;
    view.Format = DXGI_FORMAT_R32_TYPELESS;
    view.Buffer.NumElements = words;
    view.Buffer.Flags = D3D12_BUFFER_UAV_FLAG_RAW;
    device->CreateUnorderedAccessView(output.Get(), nullptr, &view, cpu);

    const UINT cases[][3] = {{down, 1, 1}, {across, down, 1}};
    for (const auto& groups : cases) {
        Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
        ID3D12GraphicsCommandList* list = direct.list.Get();
        list->CopyBufferRegion(output.Get(), 0, seed.Get(), 0, bytes);
        compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_COPY_DEST,
                                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
        ID3D12DescriptorHeap* heaps[]{heap.Get()};
        method<BindHeaps>(list, slot_bind_heaps)(list, 1, heaps);
        method<SetRoot>(list, slot_compute_root)(list, root.Get());
        list->SetPipelineState(pipeline.Get());
        method<SetTable>(list, slot_compute_table)(list, 0, gpu);
        list->Dispatch(groups[0], groups[1], groups[2]);
        compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                    D3D12_RESOURCE_STATE_COPY_SOURCE);
        list->CopyBufferRegion(readback.Get(), 0, output.Get(), 0, bytes);
        direct.submit();
        direct.finish("dispatch over 65,535 groups");

        D3D12_RANGE read{0, SIZE_T(bytes)}, empty{};
        void* mapped = nullptr;
        check(readback->Map(0, &read, &mapped), "map group ID readback");
        std::vector<UINT> got(words);
        std::memcpy(got.data(), mapped, size_t(bytes));
        readback->Unmap(0, &empty);
        // Record x + 2y belongs to group (x, y, 0); the others stay unwritten.
        for (UINT index = 0; index < records; ++index) {
            const UINT x = groups[0] == across ? index % across : index, y = groups[0] == across ? index / across : 0;
            const bool written = x < groups[0] && y < groups[1];
            const UINT expected[4] = {written ? x : 0xdeadbeefu, written ? y : 0xdeadbeefu,
                                      written ? 0u : 0xdeadbeefu, written ? group_shader_mark : 0xdeadbeefu};
            for (UINT word = 0; word < 4; ++word) {
                if (got[index * 4 + word] != expected[word]) {
                    char message[160];
                    std::snprintf(message, sizeof(message),
                                  "dispatch (%u, %u, %u): record %u word %u holds %#x, expected %#x",
                                  groups[0], groups[1], groups[2], index, word, got[index * 4 + word], expected[word]);
                    throw std::runtime_error(message);
                }
            }
        }
    }
}

// A console program whose driver compile misses a short pipeline deadline. The
// title gets a stand-in, which writes nothing; once the compile finishes, the
// next bind runs the real pipeline. Each thread stores the end of its chain.
void late_pipeline(ID3D12Device* device) {
    constexpr UINT words = 64;
    constexpr UINT64 bytes = words * sizeof(UINT);
    D3D12_DESCRIPTOR_RANGE range{D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 1, 0, 0, 0};
    D3D12_ROOT_PARAMETER table{};
    table.ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
    table.DescriptorTable = {1, &range};
    D3D12_ROOT_SIGNATURE_DESC signature{1, &table, 0, nullptr, D3D12_ROOT_SIGNATURE_FLAG_NONE};
    ComPtr<ID3DBlob> serialized;
    check(D3D12SerializeRootSignature(&signature, D3D_ROOT_SIGNATURE_VERSION_1, &serialized, nullptr),
          "serialize late pipeline root signature");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(),
                                      IID_PPV_ARGS(&root)), "create late pipeline root signature");
    D3D12_COMPUTE_PIPELINE_STATE_DESC shader{};
    shader.pRootSignature = root.Get();
    shader.CS = {late_shader, sizeof(late_shader)};
    ComPtr<ID3D12PipelineState> pipeline;
    check(method<CreateComputePipeline>(device, device_create_compute)(device, &shader, 0, nullptr,
                                                                       IID_PPV_ARGS(&pipeline)),
          "create pipeline from the late program");

    D3D12_DESCRIPTOR_HEAP_DESC heap_desc{
        D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 1, D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    ComPtr<ID3D12DescriptorHeap> heap;
    check(device->CreateDescriptorHeap(&heap_desc, IID_PPV_ARGS(&heap)), "create late pipeline view heap");
    const D3D12_CPU_DESCRIPTOR_HANDLE cpu{method<frame_abi::CpuHandle>(heap.Get(), 9)(heap.Get())};
    const D3D12_GPU_DESCRIPTOR_HANDLE gpu{method<GpuHandle>(heap.Get(), 10)(heap.Get())};
    auto seed = buffer(device, D3D12_HEAP_TYPE_UPLOAD, bytes, D3D12_RESOURCE_STATE_GENERIC_READ);
    auto output = buffer(device, D3D12_HEAP_TYPE_DEFAULT, bytes, D3D12_RESOURCE_STATE_COPY_DEST,
                         D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
    auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, bytes, D3D12_RESOURCE_STATE_COPY_DEST);
    const std::vector<UINT> unwritten(words, 0xdeadbeefu);
    compute_fixture::upload(seed.Get(), unwritten.data(), size_t(bytes));
    D3D12_UNORDERED_ACCESS_VIEW_DESC view{};
    view.ViewDimension = D3D12_UAV_DIMENSION_BUFFER;
    view.Format = DXGI_FORMAT_R32_TYPELESS;
    view.Buffer.NumElements = words;
    view.Buffer.Flags = D3D12_BUFFER_UAV_FLAG_RAW;
    device->CreateUnorderedAccessView(output.Get(), nullptr, &view, cpu);

    // A stand-in leaves every word unwritten. Anything else must be the real result.
    for (int attempt = 0; attempt < 600; ++attempt) {
        Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
        ID3D12GraphicsCommandList* list = direct.list.Get();
        list->CopyBufferRegion(output.Get(), 0, seed.Get(), 0, bytes);
        compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_COPY_DEST,
                                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
        ID3D12DescriptorHeap* heaps[]{heap.Get()};
        method<BindHeaps>(list, slot_bind_heaps)(list, 1, heaps);
        method<SetRoot>(list, slot_compute_root)(list, root.Get());
        list->SetPipelineState(pipeline.Get());
        method<SetTable>(list, slot_compute_table)(list, 0, gpu);
        list->Dispatch(1, 1, 1);
        compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                    D3D12_RESOURCE_STATE_COPY_SOURCE);
        list->CopyBufferRegion(readback.Get(), 0, output.Get(), 0, bytes);
        direct.submit();
        direct.finish("late pipeline dispatch");

        D3D12_RANGE read{0, SIZE_T(bytes)}, empty{};
        void* mapped = nullptr;
        check(readback->Map(0, &read, &mapped), "map late pipeline readback");
        std::vector<UINT> got(words);
        std::memcpy(got.data(), mapped, size_t(bytes));
        readback->Unmap(0, &empty);
        if (got == unwritten) {
            Sleep(200);
            continue;
        }
        for (UINT word = 0; word < words; ++word) {
            if (got[word] != late_shader_expected[word]) {
                char message[128];
                std::snprintf(message, sizeof(message), "thread %u stored %#x, expected %#x", word, got[word],
                              late_shader_expected[word]);
                throw std::runtime_error(message);
            }
        }
        return;
    }
    throw std::runtime_error("the stand-in was never replaced by the finished compile");
}

void mixed_lane_scenario(ID3D12Device* device) {
    check_wave_shader(device, {lane_shader, sizeof(lane_shader)}, lane_shader_expected);
}

// A root with two SRV tables (t0, space0; t0 to t3, space1) and one root
// constant (b500), optionally with static samplers, for the console raster programs.
ComPtr<ID3D12RootSignature> two_table_root(ID3D12Device* device, const D3D12_STATIC_SAMPLER_DESC* samplers,
                                           UINT count = 1) {
    D3D12_DESCRIPTOR_RANGE1 ranges[]{
        {D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 1, 0, 0, D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE, 0},
        {D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 4, 0, 1, D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE, 0}};
    D3D12_ROOT_PARAMETER1 params[3]{};
    for (UINT i = 0; i < 2; ++i) {
        params[i].ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
        params[i].DescriptorTable = {1, &ranges[i]};
    }
    params[2].ParameterType = D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS;
    params[2].Constants = {500, 0, 1};
    D3D12_VERSIONED_ROOT_SIGNATURE_DESC signature{D3D_ROOT_SIGNATURE_VERSION_1_1};
    signature.Desc_1_1 = {3, params, samplers ? count : 0u, samplers, D3D12_ROOT_SIGNATURE_FLAG_NONE};
    ComPtr<ID3DBlob> serialized;
    check(D3D12SerializeVersionedRootSignature(&signature, &serialized, nullptr),
          "serialize two-table root signature");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(),
                                      IID_PPV_ARGS(&root)), "create two-table root signature");
    return root;
}

// The stream parts a console raster draw shares, for a vertex or a mesh pipeline.
template <class Stream>
ComPtr<ID3D12PipelineState> console_pipeline(ID3D12Device* device, Stream& parts, ID3D12RootSignature* root,
                                             D3D12_SHADER_BYTECODE pixel, DXGI_FORMAT format) {
    parts.root.payload = root;
    parts.ps.payload = pixel;
    parts.raster.payload.FillMode = D3D12_FILL_MODE_SOLID;
    parts.raster.payload.CullMode = D3D12_CULL_MODE_NONE;
    parts.raster.payload.DepthClipEnable = TRUE;
    parts.blend.payload.RenderTarget[0].RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL;
    parts.dsv.payload = DXGI_FORMAT_UNKNOWN;
    parts.formats.payload.NumRenderTargets = 1;
    parts.formats.payload.RTFormats[0] = format;
    D3D12_PIPELINE_STATE_STREAM_DESC stream{sizeof(parts), &parts};
    ComPtr<ID3D12PipelineState> pipeline;
    check(method<CreateStreamPipeline>(device, device_create_stream)(device, &stream, IID_PPV_ARGS(&pipeline)),
          "create pipeline from console raster programs");
    return pipeline;
}

// Draws a fullscreen triangle with `vertex` (the console vertex program unless
// another is given), or with `mesh` when that is given, and `pixel` under `root`, into a 16x16 target of 32-bit
// `format` cleared to zero. Table N is bound at view `tables[N]` of `view_heap`, with `sampler_heap` beside it
// when given.
// Returns the texels, row by row.
std::vector<UINT> draw_console_raster(ID3D12Device* device, ID3D12RootSignature* root, D3D12_SHADER_BYTECODE pixel,
                                      DXGI_FORMAT format, ID3D12DescriptorHeap* view_heap,
                                      std::initializer_list<UINT> tables, D3D12_SHADER_BYTECODE mesh = {},
                                      D3D12_SHADER_BYTECODE vertex = {raster_vertex, sizeof(raster_vertex)},
                                      ID3D12DescriptorHeap* sampler_heap = nullptr) {
    ComPtr<ID3D12PipelineState> pipeline;
    if (mesh.BytecodeLength) {
        frame_abi::MeshPipelineStream parts;
        parts.ms.payload = mesh;
        pipeline = console_pipeline(device, parts, root, pixel, format);
    } else {
        frame_abi::RasterPipelineStream parts;
        parts.vs.payload = vertex;
        pipeline = console_pipeline(device, parts, root, pixel, format);
    }

    constexpr UINT side = 16, pitch = 256;
    const UINT stride = device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
    const UINT64 gpu_base = method<GpuHandle>(view_heap, 10)(view_heap);
    D3D12_HEAP_PROPERTIES heap{};
    heap.Type = D3D12_HEAP_TYPE_DEFAULT;
    heap.CreationNodeMask = heap.VisibleNodeMask = 1;
    D3D12_RESOURCE_DESC target_desc{};
    target_desc.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    target_desc.Width = target_desc.Height = side;
    target_desc.DepthOrArraySize = target_desc.MipLevels = 1;
    target_desc.SampleDesc.Count = 1;
    target_desc.Format = format;
    target_desc.Flags = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;
    ComPtr<ID3D12Resource> target;
    check(device->CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &target_desc, D3D12_RESOURCE_STATE_RENDER_TARGET,
                                          nullptr, IID_PPV_ARGS(&target)), "create render target");
    auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, side * pitch, D3D12_RESOURCE_STATE_COPY_DEST);
    D3D12_DESCRIPTOR_HEAP_DESC target_heap_desc{D3D12_DESCRIPTOR_HEAP_TYPE_RTV, 1, D3D12_DESCRIPTOR_HEAP_FLAG_NONE, 0};
    ComPtr<ID3D12DescriptorHeap> target_heap;
    check(device->CreateDescriptorHeap(&target_heap_desc, IID_PPV_ARGS(&target_heap)), "create target heap");
    const D3D12_CPU_DESCRIPTOR_HANDLE rtv{method<frame_abi::CpuHandle>(target_heap.Get(), 9)(target_heap.Get())};
    device->CreateRenderTargetView(target.Get(), nullptr, rtv);

    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    auto* list = direct.list.Get();
    constexpr float clear[4]{};
    method<frame_abi::Clear>(list, slot_clear_target)(list, rtv, clear, 0, nullptr);
    method<SetRoot>(list, slot_graphics_root)(list, root);
    ID3D12DescriptorHeap* bound[]{view_heap, sampler_heap};
    method<BindHeaps>(list, slot_bind_heaps)(list, sampler_heap ? 2 : 1, bound);
    UINT parameter = 0;
    for (const UINT view : tables)
        method<SetTable>(list, slot_graphics_table)(list, parameter++, {gpu_base + UINT64(view) * stride});
    method<SetTargets>(list, slot_targets)(list, 1, &rtv, FALSE, nullptr);
    const D3D12_VIEWPORT viewport{0, 0, float(side), float(side), 0, 1};
    const D3D12_RECT scissor{0, 0, LONG(side), LONG(side)};
    list->RSSetViewports(1, &viewport);
    list->RSSetScissorRects(1, &scissor);
    list->SetPipelineState(pipeline.Get());
    if (mesh.BytecodeLength) {
        method<DispatchMesh>(list, slot_dispatch_mesh)(list, 1, 1, 1);
    } else {
        list->IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        list->DrawInstanced(3, 1, 0, 0);
    }
    compute_fixture::transition(list, target.Get(), D3D12_RESOURCE_STATE_RENDER_TARGET,
                                D3D12_RESOURCE_STATE_COPY_SOURCE);
    D3D12_TEXTURE_COPY_LOCATION from{}, to{};
    from.pResource = target.Get();
    from.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
    to.pResource = readback.Get();
    to.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
    to.PlacedFootprint = {0, {format, side, side, 1, pitch}};
    method<frame_abi::Copy>(list, slot_copy_texture)(list, &to, 0, 0, 0, &from, nullptr, 0);
    direct.submit();
    direct.finish("console raster draw");

    std::vector<UINT> rows(side * pitch / 4), texels(side * side);
    D3D12_RANGE range{0, side * pitch}, empty{};
    void* mapped{};
    check(readback->Map(0, &range, &mapped), "map render target readback");
    std::memcpy(rows.data(), mapped, side * pitch);
    readback->Unmap(0, &empty);
    for (UINT y = 0; y < side; ++y)
        std::copy_n(&rows[y * pitch / 4], side, &texels[y * side]);
    return texels;
}

// A console pixel program writes a lane-spill area with v_writelane_b32, then
// runs an f16 interpolator whose attribute selector reads the same number as that
// register (`spill_fixture.py`). The selector is not a register read, so the
// program translates and writes its constant. A translator that takes it for one
// refuses the program: the pipeline becomes a stand-in, the draw writes nothing,
// every texel keeps its clear value at 0, and the scenario throws.
void lane_spill_interp(ID3D12Device* device) {
    ComPtr<ID3D12RootSignature> root;
    D3D12_ROOT_SIGNATURE_DESC signature{};
    ComPtr<ID3DBlob> serialized;
    check(D3D12SerializeRootSignature(&signature, D3D_ROOT_SIGNATURE_VERSION_1, &serialized, nullptr),
          "serialize empty root signature");
    check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(),
                                      IID_PPV_ARGS(&root)), "create empty root signature");

    // One null view; no table is bound, so the program declares no descriptor.
    D3D12_DESCRIPTOR_HEAP_DESC view_desc{D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 1,
                                         D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 1};
    ComPtr<ID3D12DescriptorHeap> view_heap;
    check(device->CreateDescriptorHeap(&view_desc, IID_PPV_ARGS(&view_heap)), "create view heap");
    const SIZE_T cpu_base = method<frame_abi::CpuHandle>(view_heap.Get(), 9)(view_heap.Get());
    D3D12_SHADER_RESOURCE_VIEW_DESC srv{};
    srv.Format = DXGI_FORMAT_R32_TYPELESS;
    srv.ViewDimension = D3D12_SRV_DIMENSION_BUFFER;
    srv.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    srv.Buffer = {0, 0, 0, D3D12_BUFFER_SRV_FLAG_RAW};
    device->CreateShaderResourceView(nullptr, &srv, {cpu_base});

    const auto texels = draw_console_raster(device, root.Get(), {lane_spill_pixel, sizeof(lane_spill_pixel)},
                                            DXGI_FORMAT_R32_UINT, view_heap.Get(), {});
    for (UINT i = 0; i < texels.size(); ++i) {
        const UINT found = texels[i];
        if (found == lane_spill_expected) continue;
        char message[200];
        if (!found)
            std::snprintf(message, sizeof(message), "texel %u,%u is 0: a stand-in pipeline or a skipped draw", i % 16, i / 16);
        else
            std::snprintf(message, sizeof(message), "texel %u,%u: expected %#x, found %#x", i % 16, i / 16,
                          lane_spill_expected, found);
        throw std::runtime_error(message);
    }
}

// A console pixel program reads a descriptor through the second of two root
// tables. Its FSSM records and PSV binding name it t3, space2 at slot 2 of
// table 1, which the title's root declares as t2, space1. The provider must
// build the root the program was compiled against (`raster_fixture.py`) and
// draw with it; under the title's root the pipeline becomes a stand-in and the
// target keeps its clear value.
void raster_tables(ID3D12Device* device) {
    auto root = two_table_root(device, nullptr);

    // Sixteen raw views. Table 0 starts at view 0 and table 1 at view 8, so the
    // program's descriptor is view 10. Every other view holds a decoy naming it.
    constexpr UINT views = 16, view_bytes = 256, side = 16;
    std::vector<UINT> words(views * view_bytes / 4);
    for (UINT i = 0; i < views; ++i) words[i * view_bytes / 4] = i == 10 ? raster_expected : raster_decoy | i;
    auto data = buffer(device, D3D12_HEAP_TYPE_UPLOAD, words.size() * 4, D3D12_RESOURCE_STATE_GENERIC_READ);
    compute_fixture::upload(data.Get(), words.data(), words.size() * 4);
    D3D12_DESCRIPTOR_HEAP_DESC view_desc{D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, views,
                                         D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 1};
    ComPtr<ID3D12DescriptorHeap> view_heap;
    check(device->CreateDescriptorHeap(&view_desc, IID_PPV_ARGS(&view_heap)), "create view heap");
    const UINT stride = device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
    const SIZE_T cpu_base = method<frame_abi::CpuHandle>(view_heap.Get(), 9)(view_heap.Get());
    for (UINT i = 0; i < views; ++i) {
        D3D12_SHADER_RESOURCE_VIEW_DESC srv{};
        srv.Format = DXGI_FORMAT_R32_TYPELESS;
        srv.ViewDimension = D3D12_SRV_DIMENSION_BUFFER;
        srv.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
        srv.Buffer = {UINT64(i) * view_bytes / 4, view_bytes / 4, 0, D3D12_BUFFER_SRV_FLAG_RAW};
        device->CreateShaderResourceView(data.Get(), &srv, {cpu_base + SIZE_T(i) * stride});
    }

    const auto texels = draw_console_raster(device, root.Get(), {raster_pixel, sizeof(raster_pixel)},
                                            DXGI_FORMAT_R32_UINT, view_heap.Get(), {0, 8});
    for (UINT i = 0; i < texels.size(); ++i) {
        const UINT found = texels[i], x = i % side, y = i / side;
        if (found == raster_expected) continue;
        char message[160];
        if (!found)
            std::snprintf(message, sizeof(message), "texel %u,%u is 0: a stand-in pipeline or a skipped draw", x, y);
        else if ((found & ~0xffu) == raster_decoy)
            std::snprintf(message, sizeof(message), "texel %u,%u read view %u, not table 1 slot 2 (view 10)", x, y,
                          found & 0xffu);
        else
            std::snprintf(message, sizeof(message), "texel %u,%u: expected %#x, found %#x", x, y,
                          raster_expected, found);
        throw std::runtime_error(message);
    }
}

// A console vertex program takes a structured buffer's descriptor from the
// resource heap, under a root with no parameters, and reads its position at its
// vertex number (`raster_fixture.py`). That load needs the stride of the view at
// that heap slot. The provider must translate the pair against a root with its
// stride table and bind it for the draw. Without it the pipeline is refused or
// becomes a stand-in, and with another stride the triangle misses the target.
void raster_heap_stride(ID3D12Device* device) {
    D3D12_VERSIONED_ROOT_SIGNATURE_DESC signature{};
    signature.Version = D3D_ROOT_SIGNATURE_VERSION_1_1;
    signature.Desc_1_1 = {0, nullptr, 0, nullptr, D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED};
    ComPtr<ID3DBlob> serialized;
    check(D3D12SerializeVersionedRootSignature(&signature, &serialized, nullptr), "serialize heap stride root");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(),
                                      IID_PPV_ARGS(&root)), "create heap stride root");

    auto data = buffer(device, D3D12_HEAP_TYPE_UPLOAD, sizeof(heap_stride_records),
                       D3D12_RESOURCE_STATE_GENERIC_READ);
    compute_fixture::upload(data.Get(), heap_stride_records, sizeof(heap_stride_records));
    D3D12_DESCRIPTOR_HEAP_DESC view_desc{D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 128,
                                         D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    ComPtr<ID3D12DescriptorHeap> view_heap;
    check(device->CreateDescriptorHeap(&view_desc, IID_PPV_ARGS(&view_heap)), "create heap stride view heap");
    const UINT stride = device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
    const SIZE_T cpu_base = method<frame_abi::CpuHandle>(view_heap.Get(), 9)(view_heap.Get());
    D3D12_SHADER_RESOURCE_VIEW_DESC srv{};
    srv.Format = DXGI_FORMAT_UNKNOWN;
    srv.ViewDimension = D3D12_SRV_DIMENSION_BUFFER;
    srv.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    srv.Buffer = {0, UINT(sizeof(heap_stride_records)) / heap_stride_bytes, heap_stride_bytes,
                  D3D12_BUFFER_SRV_FLAG_NONE};
    device->CreateShaderResourceView(data.Get(), &srv, {cpu_base + SIZE_T(heap_stride_index) * stride});

    const auto texels = draw_console_raster(device, root.Get(), {heap_stride_pixel, sizeof(heap_stride_pixel)},
                                            DXGI_FORMAT_R32_UINT, view_heap.Get(), {}, {},
                                            {heap_stride_vertex, sizeof(heap_stride_vertex)});
    for (UINT i = 0; i < texels.size(); ++i) {
        if (texels[i] == heap_stride_expected) continue;
        char message[200];
        if (!texels[i])
            std::snprintf(message, sizeof(message),
                          "texel %u,%u is 0: a stand-in pipeline, a skipped draw or a wrong buffer stride", i % 16, i / 16);
        else
            std::snprintf(message, sizeof(message), "texel %u,%u: expected %#x, found %#x", i % 16, i / 16,
                          heap_stride_expected, texels[i]);
        throw std::runtime_error(message);
    }
}

// A root-less console compute program reads a structured buffer in the resource
// heap by thread number and stores the word it found back through a second
// structured buffer at the same thread index (`shader_fixture.py`). The load is
// an IDXEN raw access whose record stride the console stores in the descriptor's
// word 1, which the program carries no way to carry: the provider must retry the
// translation against a root with the stride table appended and bind that table
// for the dispatch. A refused pipeline, a skipped dispatch or another stride
// leaves the output at its decoy.
void compute_heap_stride(ID3D12Device* device) {
    D3D12_VERSIONED_ROOT_SIGNATURE_DESC signature{};
    signature.Version = D3D_ROOT_SIGNATURE_VERSION_1_1;
    signature.Desc_1_1 = {0, nullptr, 0, nullptr, D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED};
    ComPtr<ID3DBlob> serialized;
    check(D3D12SerializeVersionedRootSignature(&signature, &serialized, nullptr), "serialize heap stride root");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(),
                                      IID_PPV_ARGS(&root)), "create heap stride root");

    constexpr UINT records = compute_heap_stride_records, record_bytes = compute_heap_stride_bytes;
    constexpr UINT words = records * (record_bytes / sizeof(UINT));
    constexpr UINT64 bytes = UINT64(words) * sizeof(UINT);
    // Each record starts with a marker and ends with a decoy that must stay.
    constexpr UINT decoy = 0xD0000000u;
    std::vector<UINT> input_words(words, decoy);
    for (UINT i = 0; i < records; ++i) input_words[i * (record_bytes / sizeof(UINT))] = 0x10000000u | (i + 1u);
    auto input = buffer(device, D3D12_HEAP_TYPE_UPLOAD, bytes, D3D12_RESOURCE_STATE_GENERIC_READ);
    compute_fixture::upload(input.Get(), input_words.data(), size_t(bytes));
    std::vector<UINT> decoy_words(words, decoy);
    auto seed = buffer(device, D3D12_HEAP_TYPE_UPLOAD, bytes, D3D12_RESOURCE_STATE_GENERIC_READ);
    compute_fixture::upload(seed.Get(), decoy_words.data(), size_t(bytes));
    auto output = buffer(device, D3D12_HEAP_TYPE_DEFAULT, bytes, D3D12_RESOURCE_STATE_COPY_DEST,
                         D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
    auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, bytes, D3D12_RESOURCE_STATE_COPY_DEST);

    D3D12_DESCRIPTOR_HEAP_DESC view_desc{D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 128,
                                         D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    ComPtr<ID3D12DescriptorHeap> view_heap;
    check(device->CreateDescriptorHeap(&view_desc, IID_PPV_ARGS(&view_heap)), "create heap stride view heap");
    const UINT stride = device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
    const SIZE_T cpu_base = method<frame_abi::CpuHandle>(view_heap.Get(), 9)(view_heap.Get());
    D3D12_SHADER_RESOURCE_VIEW_DESC srv{};
    srv.Format = DXGI_FORMAT_UNKNOWN;
    srv.ViewDimension = D3D12_SRV_DIMENSION_BUFFER;
    srv.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    srv.Buffer = {0, records, record_bytes, D3D12_BUFFER_SRV_FLAG_NONE};
    device->CreateShaderResourceView(input.Get(), &srv, {cpu_base + SIZE_T(compute_heap_stride_input_slot) * stride});
    D3D12_UNORDERED_ACCESS_VIEW_DESC uav{};
    uav.Format = DXGI_FORMAT_UNKNOWN;
    uav.ViewDimension = D3D12_UAV_DIMENSION_BUFFER;
    uav.Buffer = {0, records, record_bytes, 0, D3D12_BUFFER_UAV_FLAG_NONE};
    device->CreateUnorderedAccessView(output.Get(), nullptr, &uav, {cpu_base + SIZE_T(compute_heap_stride_output_slot) * stride});

    D3D12_COMPUTE_PIPELINE_STATE_DESC shader{};
    shader.pRootSignature = root.Get();
    shader.CS = {compute_heap_stride_shader, sizeof(compute_heap_stride_shader)};
    ComPtr<ID3D12PipelineState> pipeline;
    check(method<CreateComputePipeline>(device, device_create_compute)(device, &shader, 0, nullptr,
                                                                       IID_PPV_ARGS(&pipeline)),
          "create compute heap stride pipeline");

    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    auto list = direct.list.Get();
    list->CopyBufferRegion(output.Get(), 0, seed.Get(), 0, bytes);
    compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_COPY_DEST,
                                D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    ID3D12DescriptorHeap* heaps[]{view_heap.Get()};
    method<BindHeaps>(list, slot_bind_heaps)(list, 1, heaps);
    method<SetRoot>(list, slot_compute_root)(list, root.Get());
    list->SetPipelineState(pipeline.Get());
    list->Dispatch(1, 1, 1);
    compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                D3D12_RESOURCE_STATE_COPY_SOURCE);
    list->CopyBufferRegion(readback.Get(), 0, output.Get(), 0, bytes);
    direct.submit();
    direct.finish("dispatch console heap stride compute");

    D3D12_RANGE read{0, SIZE_T(bytes)}, empty{};
    void* mapped = nullptr;
    check(readback->Map(0, &read, &mapped), "map heap stride readback");
    std::vector<UINT> got(words);
    std::memcpy(got.data(), mapped, size_t(bytes));
    readback->Unmap(0, &empty);
    for (UINT i = 0; i < records; ++i) {
        const UINT base = i * (record_bytes / sizeof(UINT));
        const UINT expected = 0x10000000u | (i + 1u);
        if (got[base] == expected
            && std::all_of(got.begin() + base + 1, got.begin() + base + record_bytes / sizeof(UINT),
                           [decoy](UINT word) { return word == decoy; })) continue;
        char message[200];
        std::snprintf(message, sizeof(message),
                      "record %u: expected marker %#x, found %#x", i, expected, got[base]);
        throw std::runtime_error(message);
    }
}

// A point-filtered, clamped static sampler at register `slot`, space0.
D3D12_STATIC_SAMPLER_DESC point_sampler(UINT slot) {
    D3D12_STATIC_SAMPLER_DESC point{};
    point.Filter = D3D12_FILTER_MIN_MAG_MIP_POINT;
    point.AddressU = point.AddressV = point.AddressW = D3D12_TEXTURE_ADDRESS_MODE_CLAMP;
    point.MaxAnisotropy = 1;
    point.ComparisonFunc = D3D12_COMPARISON_FUNC_NEVER;
    point.BorderColor = D3D12_STATIC_BORDER_COLOR_TRANSPARENT_BLACK;
    point.MaxLOD = 12;
    point.ShaderRegister = slot;
    point.ShaderVisibility = D3D12_SHADER_VISIBILITY_ALL;
    return point;
}

// The 2x1 texture the sampler programs read, left in `state`.
ComPtr<ID3D12Resource> sampled_texture(ID3D12Device* device, D3D12_RESOURCE_STATES state) {
    D3D12_HEAP_PROPERTIES heap{};
    heap.Type = D3D12_HEAP_TYPE_DEFAULT;
    heap.CreationNodeMask = heap.VisibleNodeMask = 1;
    D3D12_RESOURCE_DESC desc{};
    desc.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    desc.Width = std::size(raster_sampler_texels);
    desc.Height = desc.DepthOrArraySize = desc.MipLevels = 1;
    desc.SampleDesc.Count = 1;
    desc.Format = DXGI_FORMAT_R32_FLOAT;
    ComPtr<ID3D12Resource> texture;
    check(device->CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST,
                                          nullptr, IID_PPV_ARGS(&texture)), "create sampled texture");
    constexpr UINT pitch = 256;
    auto upload = buffer(device, D3D12_HEAP_TYPE_UPLOAD, pitch, D3D12_RESOURCE_STATE_GENERIC_READ);
    compute_fixture::upload(upload.Get(), raster_sampler_texels, sizeof(raster_sampler_texels));
    {
        Queue copy(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
        D3D12_TEXTURE_COPY_LOCATION to{}, from{};
        to.pResource = texture.Get();
        to.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
        from.pResource = upload.Get();
        from.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
        from.PlacedFootprint = {0, {DXGI_FORMAT_R32_FLOAT, UINT(desc.Width), 1, 1, pitch}};
        method<frame_abi::Copy>(copy.list.Get(), slot_copy_texture)(copy.list.Get(), &to, 0, 0, 0, &from, nullptr, 0);
        compute_fixture::transition(copy.list.Get(), texture.Get(), D3D12_RESOURCE_STATE_COPY_DEST,
                                    state);
        copy.submit();
        copy.finish("upload sampled texture");
    }
    return texture;
}

// A console vertex program samples a texture from the resource heap with a sampler
// from the sampler heap at a constant index, the way RAGE's VfxVBuffer programs do:
// both arms of an if/else copy the sampler into vector registers, and a waterfall
// loop reads it back for the sample (`raster_fixture.py`). The sampled value is the
// position's w. The point sampler at the program's index keeps the triangle over the
// whole target; the linear ones everywhere else in the heap shrink it. A program the
// translator refuses becomes a stand-in pipeline and draws nothing.
void raster_sampler_waterfall(ID3D12Device* device) {
    D3D12_VERSIONED_ROOT_SIGNATURE_DESC signature{};
    signature.Version = D3D_ROOT_SIGNATURE_VERSION_1_1;
    signature.Desc_1_1 = {0, nullptr, 0, nullptr, D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED |
                                                  D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED};
    ComPtr<ID3DBlob> serialized;
    check(D3D12SerializeVersionedRootSignature(&signature, &serialized, nullptr), "serialize waterfall root");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(),
                                      IID_PPV_ARGS(&root)), "create waterfall root");

    auto texture = sampled_texture(device, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE |
                                           D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE);
    D3D12_DESCRIPTOR_HEAP_DESC view_desc{D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 128,
                                         D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    ComPtr<ID3D12DescriptorHeap> view_heap;
    check(device->CreateDescriptorHeap(&view_desc, IID_PPV_ARGS(&view_heap)), "create waterfall view heap");
    const UINT view_stride = device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
    const SIZE_T view_base = method<frame_abi::CpuHandle>(view_heap.Get(), 9)(view_heap.Get());
    D3D12_SHADER_RESOURCE_VIEW_DESC srv{};
    srv.Format = DXGI_FORMAT_R32_FLOAT;
    srv.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE2D;
    srv.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    srv.Texture2D.MipLevels = 1;
    device->CreateShaderResourceView(texture.Get(), &srv, {view_base + SIZE_T(waterfall_texture_index) * view_stride});

    constexpr UINT samplers = 8;
    D3D12_DESCRIPTOR_HEAP_DESC sampler_desc{D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER, samplers,
                                            D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    ComPtr<ID3D12DescriptorHeap> sampler_heap;
    check(device->CreateDescriptorHeap(&sampler_desc, IID_PPV_ARGS(&sampler_heap)), "create waterfall sampler heap");
    const UINT sampler_stride = device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER);
    const SIZE_T sampler_base = method<frame_abi::CpuHandle>(sampler_heap.Get(), 9)(sampler_heap.Get());
    for (UINT i = 0; i < samplers; ++i) {
        D3D12_SAMPLER_DESC sampler{};
        sampler.Filter = i == waterfall_sampler_index ? D3D12_FILTER_MIN_MAG_MIP_POINT : D3D12_FILTER_MIN_MAG_MIP_LINEAR;
        sampler.AddressU = sampler.AddressV = sampler.AddressW = D3D12_TEXTURE_ADDRESS_MODE_CLAMP;
        sampler.MaxAnisotropy = 1;
        sampler.ComparisonFunc = D3D12_COMPARISON_FUNC_NEVER;
        sampler.MaxLOD = 12;
        device->CreateSampler(&sampler, {sampler_base + SIZE_T(i) * sampler_stride});
    }

    const auto texels = draw_console_raster(device, root.Get(), {heap_stride_pixel, sizeof(heap_stride_pixel)},
                                            DXGI_FORMAT_R32_UINT, view_heap.Get(), {}, {},
                                            {waterfall_vertex, sizeof(waterfall_vertex)}, sampler_heap.Get());
    for (UINT i = 0; i < texels.size(); ++i) {
        if (texels[i] == heap_stride_expected) continue;
        char message[200];
        std::snprintf(message, sizeof(message),
                      "texel %u,%u is %#x: a stand-in pipeline, a skipped draw or another sampler than heap slot %u",
                      i % 16, i / 16, texels[i], waterfall_sampler_index);
        throw std::runtime_error(message);
    }
}

// Draws `pixel` (and `mesh`, when given) under `root` with the 2x1 texture the
// sampler programs read, sampled a quarter of the way between its texels, and
// requires every texel to hold the linear-filtered value.
void draw_linear_sample(ID3D12Device* device, ID3D12RootSignature* root, D3D12_SHADER_BYTECODE pixel,
                        D3D12_SHADER_BYTECODE mesh = {}) {
    auto texture = sampled_texture(device, D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE);

    // Table 0 at view 0 and table 1 at view 8, each with room for the ranges
    // the root declares: the texture is view 8, the other views are null.
    constexpr UINT views = 16;
    D3D12_DESCRIPTOR_HEAP_DESC view_desc{D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, views,
                                         D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 1};
    ComPtr<ID3D12DescriptorHeap> view_heap;
    check(device->CreateDescriptorHeap(&view_desc, IID_PPV_ARGS(&view_heap)), "create view heap");
    const UINT stride = device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
    const SIZE_T cpu_base = method<frame_abi::CpuHandle>(view_heap.Get(), 9)(view_heap.Get());
    D3D12_SHADER_RESOURCE_VIEW_DESC srv{};
    srv.Format = DXGI_FORMAT_R32_FLOAT;
    srv.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE2D;
    srv.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    srv.Texture2D.MipLevels = 1;
    for (UINT i = 0; i < views; ++i)
        device->CreateShaderResourceView(i == 8 ? texture.Get() : nullptr, &srv, {cpu_base + SIZE_T(i) * stride});

    const auto texels = draw_console_raster(device, root, pixel, DXGI_FORMAT_R32_FLOAT, view_heap.Get(), {0, 8}, mesh);
    for (UINT i = 0; i < texels.size(); ++i) {
        float found;
        std::memcpy(&found, &texels[i], sizeof(found));
        if (found == raster_sampler_linear) continue;
        char message[200];
        if (!texels[i])
            std::snprintf(message, sizeof(message), "texel %u,%u is 0: a stand-in pipeline or a skipped draw", i % 16, i / 16);
        else if (found == raster_sampler_point)
            std::snprintf(message, sizeof(message),
                          "texel %u,%u is %g: point-filtered by the root's s0, not the program's literal linear state",
                          i % 16, i / 16, found);
        else
            std::snprintf(message, sizeof(message), "texel %u,%u: expected %g, found %g", i % 16, i / 16,
                          raster_sampler_linear, found);
        throw std::runtime_error(message);
    }
}

// A console pixel program samples a texture with a linear clamped sampler it
// builds from literal words, as the console compiles a static sampler into the
// program. It declares that sampler as s0, where the title's root has a
// point-filtered static sampler in another state. The console samples with the
// literal state, so the provider must give that state a static sampler of its
// own (`inline_sampler::unmatched`). Bound to the root's s0, the draw reads the
// point-filtered value; a refused translation leaves a stand-in and the clear value.
void raster_samplers(ID3D12Device* device) {
    const auto point = point_sampler(0);
    auto root = two_table_root(device, &point);
    draw_linear_sample(device, root.Get(), {raster_sampler_pixel, sizeof(raster_sampler_pixel)});
}

// The same pixel program in a mesh pipeline. It declares s0 and s1, which the
// title's root has as point-filtered static samplers, so no declared register
// can stand in for the literal linear state and its translation against that
// root is refused. The mesh program's records name b501 where the root has b500,
// so the provider builds the pipeline a mesh root of its own; it must add the
// literal state to that root as a private static sampler and translate the
// pixel program against it. Without that the provider falls back to the pixel
// program's root-less translation, whose private sampler the root lacks: the
// pipeline becomes a stand-in and the target keeps its clear value. The mesh
// program is translated by the same runtime shader worker.
void mesh_samplers(ID3D12Device* device) {
    const D3D12_STATIC_SAMPLER_DESC points[]{point_sampler(0), point_sampler(1)};
    auto root = two_table_root(device, points, UINT(std::size(points)));
    draw_linear_sample(device, root.Get(), {mesh_sampler_pixel, sizeof(mesh_sampler_pixel)},
                       {mesh_sampler_mesh, sizeof(mesh_sampler_mesh)});
}

// The `raster-samplers` pixel program, declaring only s0, in a mesh pipeline
// under a root whose s0 is point-filtered. The translation can bind the literal
// state to the one declared register, whatever state the root gives it.
void mesh_sampler_single(ID3D12Device* device) {
    const auto point = point_sampler(0);
    auto root = two_table_root(device, &point);
    draw_linear_sample(device, root.Get(), {raster_sampler_pixel, sizeof(raster_sampler_pixel)},
                       {mesh_sampler_mesh, sizeof(mesh_sampler_mesh)});
}

// Two pixel programs in mesh pipelines under a root with no static samplers,
// as a title leaves out the samplers the console compiles into its programs.
// The translation against such a root reads the literal state from a private
// sampler, so the provider must add each program's samplers to the mesh root it
// builds: the declared s0 for the program with one sampler, a private register
// for the one that declares two. Without them the host rejects the pipeline,
// which becomes a stand-in, and the target keeps its clear value.
void mesh_sampler_rootless(ID3D12Device* device) {
    auto root = two_table_root(device, nullptr);
    for (const D3D12_SHADER_BYTECODE pixel : {D3D12_SHADER_BYTECODE{raster_sampler_pixel, sizeof(raster_sampler_pixel)},
                                              D3D12_SHADER_BYTECODE{mesh_rootless_pixel, sizeof(mesh_rootless_pixel)}})
        draw_linear_sample(device, root.Get(), pixel, {mesh_sampler_mesh, sizeof(mesh_sampler_mesh)});
}

// A console shader whose wave32 masked DPP exclusive prefix the translator must
// lower to `WavePrefixSum`. The old translator rejects it with "unrecognized
// masked DPP prologue", so this scenario fails without the wave-alloc lowering.
// The shader stores one word per lane to the UAV (the exclusive prefix of a
// wave of ones is the lane id); the scenario reads back every word and requires
// it to be the expected value, so a numerical miscompile of the lowering is
// caught here and no inter-lane store order can hide a wrong lane.
void wave_alloc_scenario(ID3D12Device* device) {
    constexpr size_t wave_alloc_words = 32;
    D3D12_ROOT_PARAMETER params[1]{};
    params[0].ParameterType = D3D12_ROOT_PARAMETER_TYPE_UAV;
    params[0].Descriptor = {0, 0};
    D3D12_ROOT_SIGNATURE_DESC signature{};
    signature.NumParameters = 1;
    signature.pParameters = params;
    ComPtr<ID3DBlob> serialized;
    check(D3D12SerializeRootSignature(&signature, D3D_ROOT_SIGNATURE_VERSION_1, &serialized, nullptr),
          "serialize wave-alloc root signature");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(),
                                      IID_PPV_ARGS(&root)), "create wave-alloc root signature");
    D3D12_COMPUTE_PIPELINE_STATE_DESC shader{};
    shader.pRootSignature = root.Get();
    shader.CS = {wave_alloc_shader, sizeof(wave_alloc_shader)};
    ComPtr<ID3D12PipelineState> pipeline;
    check(method<CreateComputePipeline>(device, device_create_compute)(device, &shader, 0, nullptr,
                                                                       IID_PPV_ARGS(&pipeline)),
          "create pipeline from the wave-alloc console shader");
    auto output = buffer(device, D3D12_HEAP_TYPE_DEFAULT, wave_alloc_words * sizeof(UINT),
                         D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
    // Seed the UAV with a sentinel so a missing store is distinguishable from the
    // expected values (0..31) in the readback.
    auto sentinel = buffer(device, D3D12_HEAP_TYPE_UPLOAD, wave_alloc_words * sizeof(UINT),
                           D3D12_RESOURCE_STATE_GENERIC_READ);
    const std::vector<UINT> fill(wave_alloc_words, kWaveAllocSentinel);
    compute_fixture::upload(sentinel.Get(), fill.data(), fill.size() * sizeof(UINT));
    auto proof = buffer(device, D3D12_HEAP_TYPE_READBACK, wave_alloc_words * sizeof(UINT),
                        D3D12_RESOURCE_STATE_COPY_DEST);
    Queue compute(device, D3D12_COMMAND_LIST_TYPE_COMPUTE);
    auto* list = compute.list.Get();
    list->CopyBufferRegion(output.Get(), 0, sentinel.Get(), 0, wave_alloc_words * sizeof(UINT));
    method<SetRoot>(list, slot_compute_root)(list, root.Get());
    list->SetPipelineState(pipeline.Get());
    method<SetUav>(list, slot_root_uav)(list, 0, output->GetGPUVirtualAddress());
    compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_COPY_DEST,
                                D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    list->Dispatch(1, 1, 1);
    compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                D3D12_RESOURCE_STATE_COPY_SOURCE);
    list->CopyBufferRegion(proof.Get(), 0, output.Get(), 0, wave_alloc_words * sizeof(UINT));
    compute.submit();
    compute.finish("dispatch of the wave-alloc console shader");
    D3D12_RANGE range{0, wave_alloc_words * sizeof(UINT)}, empty{};
    void* mapped{};
    check(proof->Map(0, &range, &mapped), "map wave-alloc proof");
    const UINT* rows = static_cast<const UINT*>(mapped);
    for (size_t i = 0; i < wave_alloc_words; ++i) {
        if (rows[i] != static_cast<UINT>(i))
            throw std::runtime_error("wave-alloc shader stored the wrong prefix result at lane " + std::to_string(i) + " (or stored nothing)");
    }
    proof->Unmap(0, &empty);
}

// Two root-less console compute programs under a root with three descriptor
// tables, which the provider cannot reflect. Each program's FSSM record and PSV
// binding name its UAV u3, space2; the root declares that descriptor, slot 2 of
// table 1, as u2, space1 (`shader_fixture.py`). The first program must be
// translated against this root and write its word to view 10. The second stores
// through slot 5, past the table, so nothing can translate it for this root and
// it gets a stand-in that writes nothing. A translation made without the root
// cannot be bound, so falling back to one leaves view 10 with its decoy.
void compute_root(ID3D12Device* device) {
    D3D12_DESCRIPTOR_RANGE ranges[]{
        {D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 1, 0, 0, 0},
        {D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 4, 0, 1, 0},
        {D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 1, 0, 3, 0}};
    D3D12_ROOT_PARAMETER params[3]{};
    for (UINT i = 0; i < 3; ++i) {
        params[i].ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
        params[i].DescriptorTable = {1, &ranges[i]};
    }
    D3D12_ROOT_SIGNATURE_DESC signature{3, params, 0, nullptr, D3D12_ROOT_SIGNATURE_FLAG_NONE};
    ComPtr<ID3DBlob> serialized;
    check(D3D12SerializeRootSignature(&signature, D3D_ROOT_SIGNATURE_VERSION_1, &serialized, nullptr),
          "serialize three-table root signature");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(),
                                      IID_PPV_ARGS(&root)), "create three-table root signature");
    auto create = [&](const unsigned char* code, size_t size, ComPtr<ID3D12PipelineState>& pipeline) {
        D3D12_COMPUTE_PIPELINE_STATE_DESC shader{};
        shader.pRootSignature = root.Get();
        shader.CS = {code, size};
        return method<CreateComputePipeline>(device, device_create_compute)(device, &shader, 0, nullptr,
                                                                            IID_PPV_ARGS(&pipeline));
    };
    ComPtr<ID3D12PipelineState> pipeline, refused;
    check(create(root_shader, sizeof(root_shader), pipeline), "create pipeline from a root-less console program");
    // With stand-ins disabled the provider fails this creation instead.
    const bool refused_created = SUCCEEDED(create(root_refused_shader, sizeof(root_refused_shader), refused));

    // Sixteen raw views of one buffer. Table 0 starts at view 0, table 1 at view
    // 8 and table 2 at view 12, so the first program's descriptor is view 10.
    // Every view starts with a decoy naming it.
    constexpr UINT views = 16, view_words = 64, decoy = 0xD0000000;
    constexpr UINT64 bytes = views * view_words * sizeof(UINT);
    std::vector<UINT> words(views * view_words);
    for (UINT i = 0; i < views; ++i) words[i * view_words] = decoy | i;
    auto seed = buffer(device, D3D12_HEAP_TYPE_UPLOAD, bytes, D3D12_RESOURCE_STATE_GENERIC_READ);
    compute_fixture::upload(seed.Get(), words.data(), size_t(bytes));
    auto output = buffer(device, D3D12_HEAP_TYPE_DEFAULT, bytes, D3D12_RESOURCE_STATE_COPY_DEST,
                         D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
    auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, bytes, D3D12_RESOURCE_STATE_COPY_DEST);
    D3D12_DESCRIPTOR_HEAP_DESC heap_desc{D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, views,
                                         D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    ComPtr<ID3D12DescriptorHeap> heap;
    check(device->CreateDescriptorHeap(&heap_desc, IID_PPV_ARGS(&heap)), "create root view heap");
    const UINT stride = device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
    const SIZE_T cpu_base = method<frame_abi::CpuHandle>(heap.Get(), 9)(heap.Get());
    const UINT64 gpu_base = method<GpuHandle>(heap.Get(), 10)(heap.Get());
    for (UINT i = 0; i < views; ++i) {
        D3D12_UNORDERED_ACCESS_VIEW_DESC view{};
        view.ViewDimension = D3D12_UAV_DIMENSION_BUFFER;
        view.Format = DXGI_FORMAT_R32_TYPELESS;
        view.Buffer.FirstElement = UINT64(i) * view_words;
        view.Buffer.NumElements = view_words;
        view.Buffer.Flags = D3D12_BUFFER_UAV_FLAG_RAW;
        device->CreateUnorderedAccessView(output.Get(), nullptr, &view, {cpu_base + SIZE_T(i) * stride});
    }

    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    ID3D12GraphicsCommandList* list = direct.list.Get();
    list->CopyBufferRegion(output.Get(), 0, seed.Get(), 0, bytes);
    compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_COPY_DEST,
                                D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    ID3D12DescriptorHeap* heaps[]{heap.Get()};
    method<BindHeaps>(list, slot_bind_heaps)(list, 1, heaps);
    method<SetRoot>(list, slot_compute_root)(list, root.Get());
    constexpr UINT first_view[]{0, 8, 12};
    for (UINT i = 0; i < 3; ++i)
        method<SetTable>(list, slot_compute_table)(list, i, {gpu_base + UINT64(first_view[i]) * stride});
    if (refused_created) {
        list->SetPipelineState(refused.Get());
        list->Dispatch(1, 1, 1);
    }
    list->SetPipelineState(pipeline.Get());
    list->Dispatch(1, 1, 1);
    compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                D3D12_RESOURCE_STATE_COPY_SOURCE);
    list->CopyBufferRegion(readback.Get(), 0, output.Get(), 0, bytes);
    direct.submit();
    direct.finish("dispatch under a three-table root");

    D3D12_RANGE read{0, SIZE_T(bytes)}, empty{};
    void* mapped = nullptr;
    check(readback->Map(0, &read, &mapped), "map root readback");
    std::vector<UINT> got(views * view_words);
    std::memcpy(got.data(), mapped, size_t(bytes));
    readback->Unmap(0, &empty);
    for (UINT i = 0; i < views; ++i) {
        const UINT found = got[i * view_words];
        if (found == (i == 10 ? root_shader_expected : decoy | i)) continue;
        char message[160];
        if (i == 10 && found == (decoy | i))
            std::snprintf(message, sizeof(message), "view 10 kept its decoy: a stand-in pipeline or a skipped dispatch");
        else if (found == root_shader_expected)
            std::snprintf(message, sizeof(message), "view %u was written, not table 1 slot 2 (view 10)", i);
        else
            std::snprintf(message, sizeof(message), "view %u: expected %#x, found %#x", i,
                          i == 10 ? root_shader_expected : decoy | i, found);
        throw std::runtime_error(message);
    }
}

// Two stores under a two-table root the provider reflects only in part, with
// descriptor paging active (`shader_fixture.py`).
void compute_root_recovery(ID3D12Device* device) {
    D3D12_DESCRIPTOR_RANGE1 ranges[]{
        {D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 1, 0, 0, D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE, 0},
        {D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 1, 0, 3, D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE, 0}};
    D3D12_ROOT_PARAMETER1 params[2]{};
    for (UINT i = 0; i < 2; ++i) {
        params[i].ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
        params[i].DescriptorTable = {1, &ranges[i]};
    }
    D3D12_VERSIONED_ROOT_SIGNATURE_DESC signature{};
    signature.Version = D3D_ROOT_SIGNATURE_VERSION_1_1;
    signature.Desc_1_1 = {2, params, 0, nullptr, D3D12_ROOT_SIGNATURE_FLAG_NONE};
    ComPtr<ID3DBlob> serialized;
    check(D3D12SerializeVersionedRootSignature(&signature, &serialized, nullptr), "serialize extended root");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(),
                                      IID_PPV_ARGS(&root)), "create two-table root signature");
    auto create = [&](const unsigned char* code, size_t size, ComPtr<ID3D12PipelineState>& pipeline) {
        D3D12_COMPUTE_PIPELINE_STATE_DESC shader{};
        shader.pRootSignature = root.Get();
        shader.CS = {code, size};
        return method<CreateComputePipeline>(device, device_create_compute)(device, &shader, 0, nullptr,
                                                                            IID_PPV_ARGS(&pipeline));
    };
    ComPtr<ID3D12PipelineState> pipeline;
    check(create(recovered_root_shader, sizeof(recovered_root_shader), pipeline), "create partially reflected pipeline");
    // Table 0 starts at view 0, table 1 at view 8; the appended slot is view 10.
    constexpr UINT views = 16, view_words = 64, decoy = 0xD0000000;
    constexpr UINT64 bytes = views * view_words * sizeof(UINT);
    std::vector<UINT> words(views * view_words);
    for (UINT i = 0; i < views; ++i) words[i * view_words] = decoy | i;
    auto seed = buffer(device, D3D12_HEAP_TYPE_UPLOAD, bytes, D3D12_RESOURCE_STATE_GENERIC_READ);
    compute_fixture::upload(seed.Get(), words.data(), size_t(bytes));
    auto output = buffer(device, D3D12_HEAP_TYPE_DEFAULT, bytes, D3D12_RESOURCE_STATE_COPY_DEST,
                         D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
    auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, bytes, D3D12_RESOURCE_STATE_COPY_DEST);
    D3D12_DESCRIPTOR_HEAP_DESC heap_desc{D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 128u,
                                         D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    ComPtr<ID3D12DescriptorHeap> heap;
    check(device->CreateDescriptorHeap(&heap_desc, IID_PPV_ARGS(&heap)), "create root view heap");
    const UINT stride = device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
    const SIZE_T cpu_base = method<frame_abi::CpuHandle>(heap.Get(), 9)(heap.Get());
    const UINT64 gpu_base = method<GpuHandle>(heap.Get(), 10)(heap.Get());
    const UINT base_view = 80;
    for (UINT i = 0; i < views; ++i) {
        D3D12_UNORDERED_ACCESS_VIEW_DESC view{};
        view.ViewDimension = D3D12_UAV_DIMENSION_BUFFER;
        view.Format = DXGI_FORMAT_R32_TYPELESS;
        view.Buffer.FirstElement = UINT64(i) * view_words;
        view.Buffer.NumElements = view_words;
        view.Buffer.Flags = D3D12_BUFFER_UAV_FLAG_RAW;
        device->CreateUnorderedAccessView(output.Get(), nullptr, &view, {cpu_base + SIZE_T(base_view + i) * stride});
    }

    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    ID3D12GraphicsCommandList* list = direct.list.Get();
    list->CopyBufferRegion(output.Get(), 0, seed.Get(), 0, bytes);
    compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_COPY_DEST,
                                D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    ID3D12DescriptorHeap* heaps[]{heap.Get()};
    method<BindHeaps>(list, slot_bind_heaps)(list, 1, heaps);
    method<SetRoot>(list, slot_compute_root)(list, root.Get());
    constexpr UINT first_view[]{0, 8};
    for (UINT i = 0; i < 2; ++i)
        method<SetTable>(list, slot_compute_table)(list, i, {gpu_base + UINT64(base_view + first_view[i]) * stride});
    list->SetPipelineState(pipeline.Get());
    list->Dispatch(1, 1, 1);
    compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                D3D12_RESOURCE_STATE_COPY_SOURCE);
    list->CopyBufferRegion(readback.Get(), 0, output.Get(), 0, bytes);
    direct.submit();
    direct.finish("dispatch under a two-table root");

    D3D12_RANGE read{0, SIZE_T(bytes)}, empty{};
    void* mapped = nullptr;
    check(readback->Map(0, &read, &mapped), "map root readback");
    std::vector<UINT> got(views * view_words);
    std::memcpy(got.data(), mapped, size_t(bytes));
    readback->Unmap(0, &empty);
    {
        for (UINT i = 0; i < views; ++i)
            for (UINT j = 1; j < view_words; ++j)
                if (got[i * view_words + j] != 0)
                    throw std::runtime_error("recovery modified a guard word");
    }
    for (UINT i = 0; i < views; ++i) {
        const UINT found = got[i * view_words];
        const bool written = i == 0 || i == 10;
        if (found == (written ? root_shader_expected : decoy | i)) continue;
        char message[160];
        if (written && found == (decoy | i))
            std::snprintf(message, sizeof(message), "view %u kept its decoy: a stand-in pipeline or a skipped dispatch", i);
        else if (found == root_shader_expected)
            std::snprintf(message, sizeof(message), "view %u was written unexpectedly", i);
        else
            std::snprintf(message, sizeof(message), "view %u: expected %#x, found %#x", i,
                          written ? root_shader_expected : decoy | i, found);
        throw std::runtime_error(message);
    }
}

// A console program under a root of three tables and a root SRV, the layout
// of some retail compute roots (`shader_fixture.py`). Its records name that
// root SRV and a raw UAV at slot 2 of table 1, where the title's root declares
// four SRVs; the title fills the table as the program was compiled to use it.
// The provider must bind the layout the program's own records give. Against
// the title's declaration the store has no UAV, so nothing translates the
// program and view 10 keeps its decoy (or, with stand-ins disabled, the
// pipeline is not created).
void compute_root_srv(ID3D12Device* device) {
    D3D12_DESCRIPTOR_RANGE1 ranges[]{
        {D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 1, 0, 0, D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE, 0},
        {D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 4, 0, 1, D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE, 0},
        {D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 1, 0, 3, D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE, 0}};
    D3D12_ROOT_PARAMETER1 params[4]{};
    for (UINT i = 0; i < 3; ++i) {
        params[i].ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
        params[i].DescriptorTable = {1, &ranges[i]};
    }
    // The program's record names this parameter t5, space7.
    params[3].ParameterType = D3D12_ROOT_PARAMETER_TYPE_SRV;
    params[3].Descriptor = {1, 3, D3D12_ROOT_DESCRIPTOR_FLAG_NONE};
    D3D12_VERSIONED_ROOT_SIGNATURE_DESC signature{};
    signature.Version = D3D_ROOT_SIGNATURE_VERSION_1_1;
    signature.Desc_1_1 = {4, params, 0, nullptr, D3D12_ROOT_SIGNATURE_FLAG_NONE};
    ComPtr<ID3DBlob> serialized;
    check(D3D12SerializeVersionedRootSignature(&signature, &serialized, nullptr), "serialize root with a root SRV");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(),
                                      IID_PPV_ARGS(&root)), "create root signature with a root SRV");
    D3D12_COMPUTE_PIPELINE_STATE_DESC shader{};
    shader.pRootSignature = root.Get();
    shader.CS = {root_srv_shader, sizeof(root_srv_shader)};
    ComPtr<ID3D12PipelineState> pipeline;
    check(method<CreateComputePipeline>(device, device_create_compute)(device, &shader, 0, nullptr,
                                                                       IID_PPV_ARGS(&pipeline)),
          "create pipeline storing through a slot the root declares an SRV");

    // Sixteen raw UAVs of one buffer. Table 0 starts at view 0, table 1 at view
    // 8 and table 2 at view 12, so the program writes view 10. Every view
    // starts with a decoy naming it.
    constexpr UINT views = 16, view_words = 64, decoy = 0xD0000000, written_view = 10;
    constexpr UINT64 bytes = views * view_words * sizeof(UINT);
    std::vector<UINT> words(views * view_words);
    for (UINT i = 0; i < views; ++i) words[i * view_words] = decoy | i;
    auto seed = buffer(device, D3D12_HEAP_TYPE_UPLOAD, bytes, D3D12_RESOURCE_STATE_GENERIC_READ);
    compute_fixture::upload(seed.Get(), words.data(), size_t(bytes));
    auto output = buffer(device, D3D12_HEAP_TYPE_DEFAULT, bytes, D3D12_RESOURCE_STATE_COPY_DEST,
                         D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
    auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, bytes, D3D12_RESOURCE_STATE_COPY_DEST);
    D3D12_DESCRIPTOR_HEAP_DESC heap_desc{D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, views,
                                         D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    ComPtr<ID3D12DescriptorHeap> heap;
    check(device->CreateDescriptorHeap(&heap_desc, IID_PPV_ARGS(&heap)), "create root view heap");
    const UINT stride = device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
    const SIZE_T cpu_base = method<frame_abi::CpuHandle>(heap.Get(), 9)(heap.Get());
    const UINT64 gpu_base = method<GpuHandle>(heap.Get(), 10)(heap.Get());
    for (UINT i = 0; i < views; ++i) {
        D3D12_UNORDERED_ACCESS_VIEW_DESC view{};
        view.ViewDimension = D3D12_UAV_DIMENSION_BUFFER;
        view.Format = DXGI_FORMAT_R32_TYPELESS;
        view.Buffer.FirstElement = UINT64(i) * view_words;
        view.Buffer.NumElements = view_words;
        view.Buffer.Flags = D3D12_BUFFER_UAV_FLAG_RAW;
        device->CreateUnorderedAccessView(output.Get(), nullptr, &view, {cpu_base + SIZE_T(i) * stride});
    }

    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    ID3D12GraphicsCommandList* list = direct.list.Get();
    list->CopyBufferRegion(output.Get(), 0, seed.Get(), 0, bytes);
    compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_COPY_DEST,
                                D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    ID3D12DescriptorHeap* heaps[]{heap.Get()};
    method<BindHeaps>(list, slot_bind_heaps)(list, 1, heaps);
    method<SetRoot>(list, slot_compute_root)(list, root.Get());
    // The program never reads through the root SRV, so the title leaves it unset.
    constexpr UINT first_view[]{0, 8, 12};
    for (UINT i = 0; i < 3; ++i)
        method<SetTable>(list, slot_compute_table)(list, i, {gpu_base + UINT64(first_view[i]) * stride});
    list->SetPipelineState(pipeline.Get());
    list->Dispatch(1, 1, 1);
    compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                D3D12_RESOURCE_STATE_COPY_SOURCE);
    list->CopyBufferRegion(readback.Get(), 0, output.Get(), 0, bytes);
    direct.submit();
    direct.finish("dispatch storing through a slot the root declares an SRV");

    D3D12_RANGE read{0, SIZE_T(bytes)}, empty{};
    void* mapped = nullptr;
    check(readback->Map(0, &read, &mapped), "map root SRV readback");
    std::vector<UINT> got(views * view_words);
    std::memcpy(got.data(), mapped, size_t(bytes));
    readback->Unmap(0, &empty);
    for (UINT i = 0; i < views; ++i) {
        for (UINT j = 1; j < view_words; ++j)
            if (got[i * view_words + j] != 0) throw std::runtime_error("the root SRV program modified a guard word");
        const UINT expected = i == written_view ? root_shader_expected : decoy | i;
        const UINT found = got[i * view_words];
        if (found == expected) continue;
        char message[160];
        if (i == written_view && found == (decoy | i))
            std::snprintf(message, sizeof(message), "view 10 kept its decoy: a stand-in pipeline or a skipped dispatch");
        else if (found == root_shader_expected)
            std::snprintf(message, sizeof(message), "view %u was written, not table 1 slot 2 (view 10)", i);
        else
            std::snprintf(message, sizeof(message), "view %u: expected %#x, found %#x", i, expected, found);
        throw std::runtime_error(message);
    }
}

ComPtr<ID3D12PipelineState> compute_pipeline(ID3D12Device* device, ComPtr<ID3D12RootSignature>& root,
                                          D3D12_SHADER_BYTECODE bytecode) {
    D3D12_ROOT_PARAMETER params[2]{};
    params[0].ParameterType = D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS;
    params[0].Constants = {1, 2, 2};
    params[1].ParameterType = D3D12_ROOT_PARAMETER_TYPE_UAV;
    params[1].Descriptor = {2, 1};
    D3D12_ROOT_SIGNATURE_DESC signature{};
    signature.NumParameters = 2;
    signature.pParameters = params;
    ComPtr<ID3DBlob> serialized;
    check(D3D12SerializeRootSignature(&signature, D3D_ROOT_SIGNATURE_VERSION_1, &serialized, nullptr),
          "serialize course root signature");
    check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(),
                                      IID_PPV_ARGS(&root)), "create course root signature");
    D3D12_COMPUTE_PIPELINE_STATE_DESC shader{};
    shader.pRootSignature = root.Get();
    shader.CS = bytecode;
    ComPtr<ID3D12PipelineState> pipeline;
    check(method<CreateComputePipeline>(device, device_create_compute)(device, &shader, 0, nullptr,
                                                                       IID_PPV_ARGS(&pipeline)),
          "create course pipeline");
    return pipeline;
}

void dispatch_wait(ID3D12Device* device) {
    constexpr size_t label = 0, payload = 65536, copied = 2 * 65536;
    constexpr UINT64 expected = 0x12345678;
    ComPtr<ID3D12RootSignature> root;
    auto pipeline = compute_pipeline(device, root, {course_dxil, sizeof(course_dxil)});

    for (bool after_dispatch : {false, true}) {
        Memory memory(3 * 65536);
        std::memset(memory.base + copied, 0xcc, sizeof(expected));
        auto output = buffer(device, D3D12_HEAP_TYPE_DEFAULT, sizeof(UINT), D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                             D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
        auto proof = buffer(device, D3D12_HEAP_TYPE_READBACK, sizeof(UINT), D3D12_RESOURCE_STATE_COPY_DEST);
        Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
        Queue compute(device, D3D12_COMMAND_LIST_TYPE_COMPUTE);
        auto* list = compute.list.Get();
        direct.write64(memory.address(payload), expected);
        direct.write64(memory.address(label), 1);
        if (!after_dispatch) compute.wait64(memory.address(label), 1);
        auto bind_course = [&] {
            const UINT constants[] = {0x5194c0d1u, 1};
            method<SetRoot>(list, slot_compute_root)(list, root.Get());
            list->SetPipelineState(pipeline.Get());
            method<SetConstants>(list, slot_root_constants)(list, 0, 2, constants, 0);
            method<SetUav>(list, slot_root_uav)(list, 1, output->GetGPUVirtualAddress());
        };
        bind_course();
        list->Dispatch(1, 1, 1);
        if (after_dispatch) compute.wait64(memory.address(label), 1);
        method<CopyMemory_>(list, slot_copy_memory)(
            list, memory.address(copied), memory.address(payload), sizeof(expected), 0);
        compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                    D3D12_RESOURCE_STATE_COPY_SOURCE);
        list->CopyBufferRegion(proof.Get(), 0, output.Get(), 0, sizeof(UINT));
        compute.submit();

        // Keep the producer unsubmitted throughout the observation window.
        const auto deadline = GetTickCount64() + 250;
        while (compute.fence->GetCompletedValue() < compute.submitted && GetTickCount64() < deadline)
            Sleep(1);
        const auto observed = compute.fence->GetCompletedValue();
        direct.submit();
        direct.finish("dispatch-wait producer");
        compute.finish("dispatch-wait consumer");
        if (observed == UINT64_MAX) throw std::runtime_error("device removed during dispatch-wait");
        expect_value(memory, label, 1, "producer label");
        expect_value(memory, payload, expected, "producer payload");
        D3D12_RANGE range{0, sizeof(UINT)}, empty{};
        void* mapped{};
        check(proof->Map(0, &range, &mapped), "map dispatch proof");
        const UINT row = *static_cast<const UINT*>(mapped);
        proof->Unmap(0, &empty);
        if (row != 1) throw std::runtime_error("course dispatch did not produce the expected first row");
        const bool early = observed >= compute.submitted;
        const UINT64 value = memory.read64(copied);
        char result[144];
        std::snprintf(result, sizeof(result), "wait=%s completed_before_producer=%s cpu=0x%08llx course=0x%x",
                      after_dispatch ? "after-dispatch" : "list-start", early ? "yes" : "no",
                      static_cast<unsigned long long>(value), row);
        std::fprintf(stderr, "%s\n", result);
        if (early || value != expected) throw std::runtime_error(result);
        if (!after_dispatch) continue;

        // Reuse the guest list. Each prefix must reach its marker before the
        // producer releases that wait, and compute bindings must survive both.
        constexpr size_t progress = 8;
        compute.reopen();
        compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_COPY_SOURCE,
                                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
        bind_course();
        list->Dispatch(1, 1, 1);
        for (UINT stage : {1u, 2u}) {
            compute.write64(memory.address(progress), stage);
            compute.wait64(memory.address(label), stage + 1);
        }
        const UINT seed = 2;
        method<SetConstants>(list, slot_root_constants)(list, 0, 1, &seed, 0);
        list->Dispatch(1, 1, 1);
        compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                    D3D12_RESOURCE_STATE_COPY_SOURCE);
        list->CopyBufferRegion(proof.Get(), 0, output.Get(), 0, sizeof(UINT));
        compute.submit();
        bool prefixes_in_order = true;
        for (UINT stage : {1u, 2u}) {
            const auto until = GetTickCount64() + deadline_ms;
            while (memory.read64(progress) < stage && GetTickCount64() < until) Sleep(1);
            prefixes_in_order &= memory.read64(progress) == stage && compute.fence->GetCompletedValue() < compute.submitted;
            direct.reopen();
            direct.write64(memory.address(label), stage + 1);
            direct.submit();
            direct.finish("release dispatch-wait stage");
        }
        compute.finish("resumed dispatch after two waits");
        if (!prefixes_in_order) throw std::runtime_error("compute prefixes did not reach their waits in order");
        check(proof->Map(0, &range, &mapped), "map resumed dispatch proof");
        const UINT resumed = *static_cast<const UINT*>(mapped);
        proof->Unmap(0, &empty);
        if ((resumed & 255) != seed) throw std::runtime_error("compute bindings lost across waits");
    }
}

ComPtr<ID3D12CommandSignature> dispatch_signature(ID3D12Device* device) {
    D3D12_INDIRECT_ARGUMENT_DESC argument{};
    argument.Type = D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH;
    D3D12_COMMAND_SIGNATURE_DESC indirect{};
    indirect.ByteStride = sizeof(D3D12_DISPATCH_ARGUMENTS);
    indirect.NumArgumentDescs = 1;
    indirect.pArgumentDescs = &argument;
    ComPtr<ID3D12CommandSignature> command;
    check(device->CreateCommandSignature(&indirect, nullptr, IID_PPV_ARGS(&command)), "create command signature");
    return command;
}

// ExecuteIndirect on its Series X|S slot runs the course shader from an
// argument buffer, and matches the same course dispatched directly.
void indirect_dispatch(ID3D12Device* device) {
    ComPtr<ID3D12RootSignature> root;
    auto pipeline = compute_pipeline(device, root, {course_dxil, sizeof(course_dxil)});

    auto command = dispatch_signature(device);
    const D3D12_DISPATCH_ARGUMENTS groups{(course_length + 7) / 8, 1, 1};
    auto arguments = buffer(device, D3D12_HEAP_TYPE_UPLOAD, sizeof(groups), D3D12_RESOURCE_STATE_GENERIC_READ);
    compute_fixture::upload(arguments.Get(), &groups, sizeof(groups));

    constexpr UINT64 bytes = sizeof(Course);
    ComPtr<ID3D12Resource> output[2], readback[2];
    Queue compute(device, D3D12_COMMAND_LIST_TYPE_COMPUTE);
    auto* list = compute.list.Get();
    const UINT constants[] = {0x5194c0d1u, course_length};
    method<SetRoot>(list, slot_compute_root)(list, root.Get());
    list->SetPipelineState(pipeline.Get());
    method<SetConstants>(list, slot_root_constants)(list, 0, 2, constants, 0);
    for (int pass = 0; pass < 2; ++pass) {
        output[pass] = buffer(device, D3D12_HEAP_TYPE_DEFAULT, bytes, D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                              D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
        readback[pass] = buffer(device, D3D12_HEAP_TYPE_READBACK, bytes, D3D12_RESOURCE_STATE_COPY_DEST);
        method<SetUav>(list, slot_root_uav)(list, 1, output[pass]->GetGPUVirtualAddress());
        if (pass == 0)
            list->Dispatch(groups.ThreadGroupCountX, 1, 1);
        else
            method<ExecuteIndirect_>(list, slot_execute_indirect)(list, command.Get(), 1, arguments.Get(), 0,
                                                                  nullptr, 0);
        compute_fixture::transition(list, output[pass].Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                    D3D12_RESOURCE_STATE_COPY_SOURCE);
        list->CopyBufferRegion(readback[pass].Get(), 0, output[pass].Get(), 0, bytes);
    }
    compute.submit();
    compute.finish("direct and indirect course dispatches");

    Course rows[2]{};
    for (int pass = 0; pass < 2; ++pass) {
        D3D12_RANGE range{0, bytes}, empty{};
        void* mapped{};
        check(readback[pass]->Map(0, &range, &mapped), "map course readback");
        std::memcpy(rows[pass].data(), mapped, bytes);
        readback[pass]->Unmap(0, &empty);
    }
    if (rows[0].front() != 0x0001) throw std::runtime_error("direct course dispatch wrote an unexpected course");
    if (rows[0] != rows[1]) throw std::runtime_error("indirect dispatch differs from the direct dispatch");
}

// A mesh dispatch signature is made exactly when the device reports mesh
// shaders. Without them it must be refused: some drivers fault while
// creating one instead of returning an error. `without` requires a device
// that reports none, as under TUXBOX_HOST_PROFILE=mesh=none.
void check_mesh_signature(ID3D12Device* device, bool without) {
    D3D12_FEATURE_DATA_D3D12_OPTIONS7 options{};
    check(device->CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS7, &options, sizeof(options)), "mesh options");
    D3D12_INDIRECT_ARGUMENT_DESC argument{};
    argument.Type = D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH_MESH;
    D3D12_COMMAND_SIGNATURE_DESC desc{sizeof(D3D12_DISPATCH_MESH_ARGUMENTS), 1, &argument, 0};
    ComPtr<ID3D12CommandSignature> command;
    const HRESULT hr = device->CreateCommandSignature(&desc, nullptr, IID_PPV_ARGS(&command));
    const bool mesh = options.MeshShaderTier != D3D12_MESH_SHADER_TIER_NOT_SUPPORTED;
    std::fprintf(stderr, "mesh-signature tier=%d result=0x%08lx\n", static_cast<int>(options.MeshShaderTier),
                 static_cast<unsigned long>(hr));
    if (mesh && (FAILED(hr) || !command))
        throw std::runtime_error("mesh dispatch signature refused on a device with mesh shaders");
    if (!mesh && (hr != E_NOTIMPL || command))
        throw std::runtime_error("mesh dispatch signature not refused on a device without mesh shaders");
    if (without && mesh) throw std::runtime_error("the device still reports mesh shaders");
}

void mesh_signature(ID3D12Device* device) { check_mesh_signature(device, false); }
void mesh_signature_none(ID3D12Device* device) { check_mesh_signature(device, true); }

// The producer and ExecuteIndirect share a compute list. Only the final
// readback waits on a CPU fence.
void gpu_indirect_dispatch(ID3D12Device* device) {
    constexpr UINT words = 24, sentinel = 0xdeadbeef;
    constexpr UINT arg_bytes = sizeof(D3D12_DISPATCH_ARGUMENTS);
    ComPtr<ID3D12RootSignature> root;
    auto pipeline = compute_pipeline(device, root, {gpu_indirect_dxil, sizeof(gpu_indirect_dxil)});
    auto command = dispatch_signature(device);
    std::array<UINT, 3 + words> initial;
    initial.fill(sentinel);
    initial[0] = 3; // Stale arguments would write all 24 positions.
    initial[1] = initial[2] = 1;
    auto seed = buffer(device, D3D12_HEAP_TYPE_UPLOAD, sizeof(initial), D3D12_RESOURCE_STATE_GENERIC_READ);
    compute_fixture::upload(seed.Get(), initial.data(), sizeof(initial));
    for (UINT groups : {0u, 1u, 2u}) {
        auto args = buffer(device, D3D12_HEAP_TYPE_DEFAULT, arg_bytes, D3D12_RESOURCE_STATE_COPY_DEST,
                           D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
        auto output = buffer(device, D3D12_HEAP_TYPE_DEFAULT, words * sizeof(UINT), D3D12_RESOURCE_STATE_COPY_DEST,
                             D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
        auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, sizeof(initial), D3D12_RESOURCE_STATE_COPY_DEST);
        Queue compute(device, D3D12_COMMAND_LIST_TYPE_COMPUTE);
        auto* list = compute.list.Get();
        list->CopyBufferRegion(args.Get(), 0, seed.Get(), 0, arg_bytes);
        list->CopyBufferRegion(output.Get(), 0, seed.Get(), arg_bytes, words * sizeof(UINT));
        compute_fixture::transition(list, args.Get(), D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
        compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
        method<SetRoot>(list, slot_compute_root)(list, root.Get());
        list->SetPipelineState(pipeline.Get());
        const UINT producer[]{1, groups};
        method<SetConstants>(list, slot_root_constants)(list, 0, 2, producer, 0);
        method<SetUav>(list, slot_root_uav)(list, 1, args->GetGPUVirtualAddress());
        list->Dispatch(1, 1, 1);
        compute_fixture::transition(list, args.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT);
        const UINT consumer = 0;
        method<SetConstants>(list, slot_root_constants)(list, 0, 1, &consumer, 0);
        method<SetUav>(list, slot_root_uav)(list, 1, output->GetGPUVirtualAddress());
        method<ExecuteIndirect_>(list, slot_execute_indirect)(list, command.Get(), 1, args.Get(), 0, nullptr, 0);
        compute_fixture::transition(list, args.Get(), D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT, D3D12_RESOURCE_STATE_COPY_SOURCE);
        compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE);
        list->CopyBufferRegion(readback.Get(), 0, args.Get(), 0, arg_bytes);
        list->CopyBufferRegion(readback.Get(), arg_bytes, output.Get(), 0, words * sizeof(UINT));
        compute.submit();
        compute.finish("GPU-written indirect dispatch");

        decltype(initial) actual{};
        D3D12_RANGE range{0, sizeof(actual)}, empty{};
        void* mapped{};
        check(readback->Map(0, &range, &mapped), "map indirect arguments and output");
        std::memcpy(actual.data(), mapped, sizeof(actual));
        readback->Unmap(0, &empty);
        if (actual[0] != groups || actual[1] != 1 || actual[2] != 1)
            throw std::runtime_error("incorrect GPU-generated dispatch arguments");
        UINT changed = 0;
        for (UINT i = 0; i < words; ++i) {
            const UINT value = actual[3 + i];
            changed += value != sentinel;
            if (value != (i < 8 * groups ? i + 1 : sentinel))
                throw std::runtime_error("GPU indirect output mismatch at group count " +
                                         std::to_string(groups) + ", word " + std::to_string(i));
        }
        std::fprintf(stderr, "gpu-indirect-dispatch groups=%u changed=%u untouched=%u\n", groups, changed, words - changed);
    }
}

// A compute program writes dispatch arguments, and in half the cases a command
// count, that an ExecuteIndirect later in the same list reads. Nothing waits on
// the CPU between the write and the read, and every round writes different
// values into the same two buffers, so an early or stale read shows in that
// round's output. The cases differ in what the list records between the write
// and the read. Each case has its own queue, so no case borrows another's
// barriers. The combined state and the lone UAV barrier are what a console
// title may record; the provider has to order the read itself.
void gpu_indirect_order(ID3D12Device* device) {
    constexpr UINT rounds = 48, words = 24, sentinel = 0xdeadbeef;
    constexpr UINT arg_bytes = sizeof(D3D12_DISPATCH_ARGUMENTS), output_bytes = rounds * words * sizeof(UINT);
    constexpr auto uav = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
    // Zero means the buffers stay in the unordered access state and only a UAV
    // barrier separates the write from the read.
    struct Case {
        const char* name;
        D3D12_RESOURCE_STATES read;
        bool counted;
    };
    const auto unchanged = static_cast<D3D12_RESOURCE_STATES>(0);
    const auto combined = static_cast<D3D12_RESOURCE_STATES>(D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT |
                                                             D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);
    const Case cases[]{
        {"transition", D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT, false},
        {"combined-state", combined, false},
        {"uav-barrier", unchanged, false},
        {"transition+count", D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT, true},
        {"combined-state+count", combined, true},
        {"uav-barrier+count", unchanged, true},
    };
    ComPtr<ID3D12RootSignature> root;
    auto pipeline = compute_pipeline(device, root, {gpu_indirect_dxil, sizeof(gpu_indirect_dxil)});
    auto command = dispatch_signature(device);
    // What the buffers hold before the batch under test: three groups, which
    // write every word of a round, and a count of one.
    std::vector<UINT> initial(6 + rounds * words, sentinel);
    initial[0] = 3;
    initial[1] = initial[2] = initial[3] = initial[4] = initial[5] = 1;
    auto seed = buffer(device, D3D12_HEAP_TYPE_UPLOAD, initial.size() * sizeof(UINT), D3D12_RESOURCE_STATE_GENERIC_READ);
    compute_fixture::upload(seed.Get(), initial.data(), initial.size() * sizeof(UINT));
    auto uav_barrier = [](ID3D12GraphicsCommandList* list) {
        D3D12_RESOURCE_BARRIER barrier{};
        barrier.Type = D3D12_RESOURCE_BARRIER_TYPE_UAV;
        list->ResourceBarrier(1, &barrier);
    };
    std::string failed;
    for (const auto& test : cases) {
        auto gpu = [&](UINT64 bytes) {
            return buffer(device, D3D12_HEAP_TYPE_DEFAULT, bytes, D3D12_RESOURCE_STATE_COPY_DEST,
                          D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
        };
        auto args = gpu(arg_bytes), count = gpu(arg_bytes), output = gpu(output_bytes);
        auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, output_bytes, D3D12_RESOURCE_STATE_COPY_DEST);
        Queue compute(device, D3D12_COMMAND_LIST_TYPE_COMPUTE);
        auto* list = compute.list.Get();
        list->CopyBufferRegion(args.Get(), 0, seed.Get(), 0, arg_bytes);
        list->CopyBufferRegion(count.Get(), 0, seed.Get(), arg_bytes, arg_bytes);
        list->CopyBufferRegion(output.Get(), 0, seed.Get(), 2 * arg_bytes, output_bytes);
        for (auto* resource : {args.Get(), count.Get(), output.Get()})
            compute_fixture::transition(list, resource, D3D12_RESOURCE_STATE_COPY_DEST, uav);
        compute.submit();
        compute.finish("seed indirect arguments");

        // One list, one submission, no CPU wait until the readback.
        compute.reopen();
        method<SetRoot>(list, slot_compute_root)(list, root.Get());
        list->SetPipelineState(pipeline.Get());
        auto groups = [](UINT round) { return round % 3; };
        auto commands = [&](UINT round) { return !test.counted || round % 4 != 3 ? 1u : 0u; };
        auto produce = [&](ID3D12Resource* target, UINT value) {
            const UINT constants[]{1, value};
            method<SetConstants>(list, slot_root_constants)(list, 0, 2, constants, 0);
            method<SetUav>(list, slot_root_uav)(list, 1, target->GetGPUVirtualAddress());
            list->Dispatch(1, 1, 1);
        };
        auto move = [&](D3D12_RESOURCE_STATES before, D3D12_RESOURCE_STATES after) {
            compute_fixture::transition(list, args.Get(), before, after);
            if (test.counted) compute_fixture::transition(list, count.Get(), before, after);
        };
        for (UINT round = 0; round < rounds; ++round) {
            produce(args.Get(), groups(round));
            if (test.counted) produce(count.Get(), commands(round));
            if (test.read != unchanged) move(uav, test.read); else uav_barrier(list);
            const UINT consumer = 0;
            method<SetConstants>(list, slot_root_constants)(list, 0, 1, &consumer, 0);
            method<SetUav>(list, slot_root_uav)(list, 1, output->GetGPUVirtualAddress() + round * words * sizeof(UINT));
            method<ExecuteIndirect_>(list, slot_execute_indirect)(list, command.Get(), 1, args.Get(), 0,
                                                                  test.counted ? count.Get() : nullptr, 0);
            if (test.read != unchanged) move(test.read, uav); else uav_barrier(list);
        }
        compute_fixture::transition(list, output.Get(), uav, D3D12_RESOURCE_STATE_COPY_SOURCE);
        list->CopyBufferRegion(readback.Get(), 0, output.Get(), 0, output_bytes);
        compute.submit();
        compute.finish("GPU-ordered indirect dispatches");

        std::vector<UINT> actual(rounds * words);
        D3D12_RANGE range{0, output_bytes}, empty{};
        void* mapped{};
        check(readback->Map(0, &range, &mapped), "map ordered indirect output");
        std::memcpy(actual.data(), mapped, output_bytes);
        readback->Unmap(0, &empty);
        // Words a round wrote: a prefix of 1, 2, 3... or, failing that, words.
        auto written = [&](UINT round) {
            const UINT* row = &actual[round * words];
            UINT n = 0;
            while (n < words && row[n] == n + 1) ++n;
            for (UINT i = n; i < words; ++i)
                if (row[i] != sentinel) return words + 1;
            return n;
        };
        auto expected = [&](UINT round) { return commands(round) * groups(round) * 8; };
        UINT wrong = 0, first = rounds, as_seeded = 0, as_previous = 0;
        for (UINT round = 0; round < rounds; ++round) {
            const UINT found = written(round);
            if (found == expected(round)) continue;
            if (!wrong++) first = round;
            as_seeded += found == words;
            as_previous += round && found == expected(round - 1);
        }
        std::fprintf(stderr, "gpu-indirect-order case=%s rounds=%u wrong=%u first=%d as-seeded=%u as-previous-round=%u\n",
                     test.name, rounds, wrong, wrong ? static_cast<int>(first) : -1, as_seeded, as_previous);
        if (wrong) failed += std::string(failed.empty() ? "" : ", ") + test.name;
    }
    if (!failed.empty()) throw std::runtime_error("indirect arguments read out of order in: " + failed);
}

// Two adjacent allocations of title memory: ordinary read-write pages, then
// pages that record CPU writes for GetWriteWatch.
struct WatchedPair {
    static constexpr size_t granule = 65536;
    BYTE* base{};
    WatchedPair() {
        // The address of a released reservation can be taken by another thread
        // before it is allocated again in two parts, so try more than once.
        for (int attempt = 0; attempt < 16 && !base; ++attempt) {
            auto* probe = static_cast<BYTE*>(VirtualAlloc(nullptr, 2 * granule, MEM_RESERVE, PAGE_NOACCESS));
            if (!probe) break;
            VirtualFree(probe, 0, MEM_RELEASE);
            void* plain = VirtualAlloc(probe, granule, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
            void* watched = plain ? VirtualAlloc(probe + granule, granule,
                                                 MEM_COMMIT | MEM_RESERVE | MEM_WRITE_WATCH, PAGE_READWRITE)
                                  : nullptr;
            if (plain && watched) base = probe;
            else if (plain) VirtualFree(plain, 0, MEM_RELEASE);
        }
        if (!base) throw std::runtime_error("allocate adjacent plain and write-watch title memory");
    }
    ~WatchedPair() {
        VirtualFree(base, 0, MEM_RELEASE);
        VirtualFree(base + granule, 0, MEM_RELEASE);
    }
    WatchedPair(const WatchedPair&) = delete;
    WatchedPair& operator=(const WatchedPair&) = delete;
    BYTE* watched() const { return base + granule; }
    // Pages of the write-watch half that record a write and lie at or past
    // `offset` bytes into it.
    ULONG_PTR written_from(size_t offset) const {
        void* pages[16];
        ULONG_PTR count = std::size(pages);
        DWORD granularity = 0;
        if (GetWriteWatch(0, watched(), granule, pages, &count, &granularity))
            throw std::runtime_error("query the write watch");
        return std::count_if(pages, pages + count, [&](void* written) {
            return static_cast<BYTE*>(written) >= watched() + offset;
        });
    }
};

// The GPU writes a guest texture whose tiled layout has padding between its
// texels, several times, while a compute queue waits for a label the last
// write sets. The provider publishes each write to the title's pages. It must
// put every texel where the console keeps it and change nothing else: not the
// padding, which can hold other title data, and not a page the title has
// closed. Whether a published write shows in the title's write watch depends
// on the host (Proton's kernel-tracked watch records it), so only pages past
// the texture must stay unrecorded.
void texture_publication(ID3D12Device* device) {
    constexpr UINT width = 100, height = 60, pitch = 512, batches = 4;
    constexpr size_t page = 4096, texture_bytes = 8 * page, words = texture_bytes / 4;
    constexpr size_t rows_bytes = size_t(pitch) * height;
    // The texture spans both allocations, and the title closes its third page.
    constexpr size_t start = WatchedPair::granule - 4 * page, closed = 2 * page;
    WatchedPair memory;
    Memory label(65536);
    auto* guest = reinterpret_cast<UINT*>(memory.base + start);
    // Every word names its own place, so a read of the texture shows where the
    // console keeps each texel, and no word equals a texel written below.
    auto initial = [](size_t word) { return 0x80000000u | static_cast<UINT>(word); };
    for (size_t word = 0; word < words; ++word) guest[word] = initial(word);

    D3D12_RESOURCE_DESC desc{};
    desc.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    desc.Width = width;
    desc.Height = height;
    desc.DepthOrArraySize = 1;
    desc.MipLevels = 1;
    desc.SampleDesc.Count = 1;
    desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    desc.Layout = static_cast<D3D12_TEXTURE_LAYOUT>(0x105);
    ComPtr<ID3D12Resource> texture;
    check(method<Place>(device, device_place)(device, reinterpret_cast<UINT64>(guest), &desc,
                                              D3D12_RESOURCE_STATE_COMMON, nullptr, IID_PPV_ARGS(&texture)),
          "place guest texture");
    D3D12_TEXTURE_COPY_LOCATION image{}, linear{};
    image.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
    image.pResource = texture.Get();
    linear.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
    linear.PlacedFootprint.Footprint = {DXGI_FORMAT_R8G8B8A8_UNORM, width, height, 1, pitch};

    // Where each texel lives: read the texture as the title's bytes define it.
    auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, rows_bytes, D3D12_RESOURCE_STATE_COPY_DEST);
    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    linear.pResource = readback.Get();
    method<frame_abi::Copy>(direct.list.Get(), slot_copy_texture)(
        direct.list.Get(), &linear, 0, 0, 0, &image, nullptr, 0);
    direct.submit();
    direct.finish("reading the guest texture");
    std::vector<size_t> home(size_t(width) * height);
    std::vector<bool> texel(words, false);
    {
        D3D12_RANGE range{0, rows_bytes}, empty{};
        void* mapped{};
        check(readback->Map(0, &range, &mapped), "map texture readback");
        for (UINT y = 0; y < height; ++y)
            for (UINT x = 0; x < width; ++x) {
                UINT value;
                std::memcpy(&value, static_cast<const BYTE*>(mapped) + size_t(y) * pitch + size_t(x) * 4, 4);
                const size_t word = value & 0x7fffffffu;
                if (!(value & 0x80000000u) || word >= words || texel[word]) {
                    readback->Unmap(0, &empty);
                    throw std::runtime_error("the texture did not read each texel from its own guest word");
                }
                texel[word] = true;
                home[size_t(y) * width + x] = word;
            }
        readback->Unmap(0, &empty);
    }
    if (std::count(texel.begin(), texel.end(), false) == 0)
        throw std::runtime_error("the texture layout has no padding to protect");

    // From here on the title makes no CPU write to the watched pages, and one
    // page of the texture is closed to every access. The watched pages past the
    // texture carry a sentinel that must survive.
    constexpr size_t watched_texture = start + texture_bytes - WatchedPair::granule;
    std::memset(memory.watched() + watched_texture, 0xa5, WatchedPair::granule - watched_texture);
    if (ResetWriteWatch(memory.watched(), WatchedPair::granule))
        throw std::runtime_error("reset the write watch");
    DWORD old{};
    if (!VirtualProtect(memory.base + start + closed, page, PAGE_NOACCESS, &old))
        throw std::runtime_error("close a page of the texture");

    auto written = [](UINT batch, UINT x, UINT y) { return 0x40000000u | batch << 16 | y << 8 | x; };
    Queue compute(device, D3D12_COMMAND_LIST_TYPE_COMPUTE);
    compute.wait64(label.address(), batches);
    compute.write64(label.address(8), 1);
    compute.submit();
    std::vector<ComPtr<ID3D12Resource>> uploads;
    std::vector<ComPtr<ID3D12CommandAllocator>> allocators;
    std::vector<ComPtr<ID3D12GraphicsCommandList>> lists;
    for (UINT batch = 1; batch <= batches; ++batch) {
        std::vector<BYTE> rows(rows_bytes);
        for (UINT y = 0; y < height; ++y)
            for (UINT x = 0; x < width; ++x) {
                const UINT value = written(batch, x, y);
                std::memcpy(rows.data() + size_t(y) * pitch + size_t(x) * 4, &value, 4);
            }
        uploads.push_back(buffer(device, D3D12_HEAP_TYPE_UPLOAD, rows_bytes, D3D12_RESOURCE_STATE_GENERIC_READ));
        compute_fixture::upload(uploads.back().Get(), rows.data(), rows_bytes);
        allocators.emplace_back();
        check(device->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, IID_PPV_ARGS(&allocators.back())),
              "create batch allocator");
        lists.push_back(open_list(device, D3D12_COMMAND_LIST_TYPE_DIRECT, allocators.back().Get()));
        auto* list = lists.back().Get();
        linear.pResource = uploads.back().Get();
        method<frame_abi::Copy>(list, slot_copy_texture)(list, &image, 0, 0, 0, &linear, nullptr, 0);
        if (batch == batches) method<Write64>(list, slot_write64)(list, label.address(), batches, 0, 0);
        check(list->Close(), "close batch list");
        ID3D12CommandList* one[]{list};
        direct.queue->ExecuteCommandLists(1, one);
        direct.signal();
    }
    direct.finish("direct queue writing the guest texture");
    compute.finish("compute queue waiting for the last texture write");
    expect_value(label, 8, 1, "compute queue released by the last texture write");

    const ULONG_PTR watched = memory.written_from(watched_texture);
    if (!VirtualProtect(memory.base + start + closed, page, old, &old))
        throw std::runtime_error("reopen the closed page");
    char message[200];
    for (UINT y = 0; y < height; ++y)
        for (UINT x = 0; x < width; ++x) {
            const size_t word = home[size_t(y) * width + x];
            const bool shut = word * 4 >= closed && word * 4 < closed + page;
            const UINT expected = shut ? initial(word) : written(batches, x, y);
            if (guest[word] != expected) {
                std::snprintf(message, sizeof(message),
                              "texel (%u, %u) at guest word %zu%s: expected %#x, found %#x", x, y, word,
                              shut ? " in the closed page" : "", expected, guest[word]);
                throw std::runtime_error(message);
            }
        }
    for (size_t word = 0; word < words; ++word)
        if (!texel[word] && guest[word] != initial(word)) {
            std::snprintf(message, sizeof(message), "padding at guest word %zu changed from %#x to %#x", word,
                          initial(word), guest[word]);
            throw std::runtime_error(message);
        }
    if (watched) {
        std::snprintf(message, sizeof(message),
                      "publishing the texture recorded writes to %llu write-watch pages past the texture",
                      static_cast<unsigned long long>(watched));
        throw std::runtime_error(message);
    }
    for (size_t at = watched_texture; at < WatchedPair::granule; ++at)
        if (memory.watched()[at] != 0xa5) throw std::runtime_error("title memory past the texture changed");
}

// CopyMemoryToMemoryX reads a guest texture back into title memory, as linear
// rows at the copyable footprint's pitch.
void texture_readback(ID3D12Device* device) {
    constexpr UINT width = 256, height = 256, pitch = 256;
    constexpr UINT64 bytes = UINT64(pitch) * height;
    Memory image(4 * 65536);
    Memory result(2 * 65536);
    D3D12_RESOURCE_DESC desc{};
    desc.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    desc.Width = width;
    desc.Height = height;
    desc.DepthOrArraySize = 1;
    desc.MipLevels = 1;
    desc.SampleDesc.Count = 1;
    desc.Format = DXGI_FORMAT_R8_UNORM;
    desc.Layout = static_cast<D3D12_TEXTURE_LAYOUT>(0x105);
    ComPtr<ID3D12Resource> texture;
    check(method<Place>(device, device_place)(device, image.address(), &desc, D3D12_RESOURCE_STATE_COMMON,
                                              nullptr, IID_PPV_ARGS(&texture)), "place guest texture");
    auto upload = buffer(device, D3D12_HEAP_TYPE_UPLOAD, bytes, D3D12_RESOURCE_STATE_GENERIC_READ);
    std::vector<BYTE> expected(bytes);
    for (size_t i = 0; i < bytes; ++i) expected[i] = static_cast<BYTE>(i * 7 + i / pitch * 3);
    compute_fixture::upload(upload.Get(), expected.data(), bytes);

    D3D12_TEXTURE_COPY_LOCATION target{}, source{};
    target.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
    target.pResource = texture.Get();
    source.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
    source.pResource = upload.Get();
    source.PlacedFootprint.Footprint = {DXGI_FORMAT_R8_UNORM, width, height, 1, pitch};
    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    method<frame_abi::Copy>(direct.list.Get(), slot_copy_texture)(direct.list.Get(), &target, 0, 0, 0, &source,
                                                                  nullptr, 0);
    // The write promoted the texture to a copy destination. It must leave that
    // state before it can be read.
    compute_fixture::transition(direct.list.Get(), texture.Get(), D3D12_RESOURCE_STATE_COPY_DEST,
                                D3D12_RESOURCE_STATE_COPY_SOURCE);
    method<CopyMemory_>(direct.list.Get(), slot_copy_memory)(direct.list.Get(), result.address(),
                                                             image.address(), bytes, 0);
    direct.submit();
    direct.finish("texture readback");
    if (std::memcmp(result.base, expected.data(), bytes))
        throw std::runtime_error("texture readback differs from the uploaded texels");
}

// `count` timed calls, in microseconds, sorted.
template<class Timed>
std::vector<double> timed_us(unsigned count, Timed timed) {
    LARGE_INTEGER frequency{}, before{}, after{};
    QueryPerformanceFrequency(&frequency);
    std::vector<double> readings(count);
    for (auto& reading : readings) {
        QueryPerformanceCounter(&before);
        timed();
        QueryPerformanceCounter(&after);
        reading = double(after.QuadPart - before.QuadPart) * 1e6 / double(frequency.QuadPart);
    }
    std::sort(readings.begin(), readings.end());
    return readings;
}

template<class Timed>
double median_us(unsigned count, Timed timed) {
    return timed_us(count, timed)[count / 2];
}

// The translation of the console shader that preparation left in the cache, or
// nothing when it has not been prepared.
std::vector<char> prepared_translation() {
    wchar_t temporary[MAX_PATH + 1]{};
    if (!GetTempPathW(MAX_PATH, temporary)) return {};
    const auto file = std::filesystem::path(temporary) / L"tuxbox_shader_cache" / L"by_source" /
                      (std::wstring(xbox_shader_digest) + L".cso");
    std::ifstream input(file, std::ios::binary);
    return {std::istreambuf_iterator<char>(input), std::istreambuf_iterator<char>()};
}

struct Readings {
    double submit_us, roundtrip_us, copy_us;
    // Zero when the console shader's translation is not prepared.
    double shader_us, dxil_us;
};

Readings measure(ID3D12Device* device) {
    Readings result{};
    Memory memory(4 * 65536);
    {
        // One list holding a fence write: record, submit and wait.
        Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
        UINT64 value = 0;
        result.submit_us = median_us(300, [&] {
            if (value) direct.reopen();
            direct.write64(memory.address(0), ++value);
            direct.submit();
            direct.finish("benchmark submission");
        });
    }
    {
        // A label handed from the direct queue to a waiting compute queue.
        Queue compute(device, D3D12_COMMAND_LIST_TYPE_COMPUTE);
        Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
        UINT64 value = 0;
        result.roundtrip_us = median_us(200, [&] {
            if (value) {
                compute.reopen();
                direct.reopen();
            }
            ++value;
            compute.wait64(memory.address(8), value);
            compute.write64(memory.address(16), value);
            compute.submit();
            direct.write64(memory.address(8), value);
            direct.submit();
            direct.finish("benchmark label writer");
            compute.finish("benchmark label waiter");
        });
    }
    {
        Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
        bool used = false;
        result.copy_us = median_us(200, [&] {
            if (used) direct.reopen();
            used = true;
            method<CopyMemory_>(direct.list.Get(), slot_copy_memory)(
                direct.list.Get(), memory.address(2 * 65536), memory.address(65536), 65536, 0);
            direct.submit();
            direct.finish("benchmark memory copy");
        });
    }
    const auto translation = prepared_translation();
    if (!translation.empty()) {
        // The same pipeline twice over: from the console shader, which the
        // provider looks up in the translation cache, and from the translation
        // itself. The difference is what the lookup costs.
        D3D12_ROOT_SIGNATURE_DESC signature{};
        ComPtr<ID3DBlob> serialized;
        check(D3D12SerializeRootSignature(&signature, D3D_ROOT_SIGNATURE_VERSION_1, &serialized, nullptr),
              "serialize empty root signature");
        ComPtr<ID3D12RootSignature> root;
        check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(),
                                          IID_PPV_ARGS(&root)), "create empty root signature");
        auto create = [&](const void* bytes, size_t size) {
            D3D12_COMPUTE_PIPELINE_STATE_DESC shader{};
            shader.pRootSignature = root.Get();
            shader.CS = {bytes, size};
            ComPtr<ID3D12PipelineState> pipeline;
            check(method<CreateComputePipeline>(device, device_create_compute)(device, &shader, 0, nullptr,
                                                                               IID_PPV_ARGS(&pipeline)),
                  "benchmark pipeline");
        };
        result.shader_us = median_us(100, [&] { create(xbox_shader, sizeof(xbox_shader)); });
        result.dxil_us = median_us(100, [&] { create(translation.data(), translation.size()); });
    }
    return result;
}

// TuxBox's XboxBvhBuilder, loaded by name as a title's builder calls reach it.
struct Builder {
    using Construct = void* (WINAPI*)(void*, ID3D12Device*, bool);
    using Destroy = void (WINAPI*)(void*);
    using Build = void (WINAPI*)(void*, ID3D12GraphicsCommandList*,
                                 const D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC*, UINT,
                                 const D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC*);
    // D3D12XBOX_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_BATCHED_DESC2.
    struct Entry {
        D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC desc;
        UINT64 report_count;
        const D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC* reports;
        UINT64 stream_bytes;
        const void* stream;
    };
    using Batch = void (WINAPI*)(void*, ID3D12GraphicsCommandList*, UINT, const Entry*);
    using CopyStructure = void (WINAPI*)(void*, ID3D12GraphicsCommandList*, D3D12_GPU_VIRTUAL_ADDRESS,
                                         D3D12_GPU_VIRTUAL_ADDRESS,
                                         D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE);
    using Prebuild = void (WINAPI*)(void*, const D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS*,
                                    const UINT64*,
                                    D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO*);

    HMODULE library;
    alignas(8) BYTE object[24]{};
    explicit Builder(ID3D12Device* device)
        : library(LoadLibraryExW(L"XboxBvhBuilder.dll", nullptr, 0)) {
        if (!library) throw std::runtime_error("load XboxBvhBuilder.dll");
        entry<Construct>(1)(object, device, false);
    }
    ~Builder() {
        entry<Destroy>(2)(object);
        FreeLibrary(library);
    }
    Builder(const Builder&) = delete;
    Builder& operator=(const Builder&) = delete;
    template<class Function> Function entry(WORD ordinal) const {
        auto address = GetProcAddress(library, MAKEINTRESOURCEA(ordinal));
        if (!address) throw std::runtime_error("missing XboxBvhBuilder export");
        return reinterpret_cast<Function>(address);
    }
};

void unordered_barrier(ID3D12GraphicsCommandList* list) {
    D3D12_RESOURCE_BARRIER barrier{};
    barrier.Type = D3D12_RESOURCE_BARRIER_TYPE_UAV;
    list->ResourceBarrier(1, &barrier);
}

// A bottom-level structure built singly with a size report, copied, and built
// again through the batched call, all by way of the console's builder library.
void ray_tracing_builder(ID3D12Device* device) {
    Builder builder(device);
    const float triangle[] = {0.f, 0.f, 0.f, 1.f, 0.f, 0.f, 0.f, 1.f, 0.f};
    auto vertices = buffer(device, D3D12_HEAP_TYPE_UPLOAD, sizeof(triangle), D3D12_RESOURCE_STATE_GENERIC_READ);
    compute_fixture::upload(vertices.Get(), triangle, sizeof(triangle));
    D3D12_RAYTRACING_GEOMETRY_DESC geometry{};
    geometry.Type = D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES;
    geometry.Flags = D3D12_RAYTRACING_GEOMETRY_FLAG_OPAQUE;
    geometry.Triangles.VertexFormat = DXGI_FORMAT_R32G32B32_FLOAT;
    geometry.Triangles.VertexCount = 3;
    geometry.Triangles.VertexBuffer = {vertices->GetGPUVirtualAddress(), 3 * sizeof(float)};
    D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS inputs{};
    inputs.Type = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL;
    inputs.Flags = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_ALLOW_COMPACTION;
    inputs.NumDescs = 1;
    inputs.pGeometryDescs = &geometry;
    D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO size{};
    builder.entry<Builder::Prebuild>(7)(builder.object, &inputs, nullptr, &size);
    if (!size.ResultDataMaxSizeInBytes || !size.ScratchDataSizeInBytes)
        throw std::runtime_error("no prebuild sizes; ray tracing is unavailable");

    auto gpu_buffer = [&](UINT64 bytes, D3D12_RESOURCE_STATES state) {
        return buffer(device, D3D12_HEAP_TYPE_DEFAULT, bytes, state,
                      D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
    };
    ComPtr<ID3D12Resource> structure[3];
    for (auto& item : structure)
        item = gpu_buffer(size.ResultDataMaxSizeInBytes,
                          D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE);
    auto scratch = gpu_buffer(size.ScratchDataSizeInBytes, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    auto reports = gpu_buffer(32, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, 32, D3D12_RESOURCE_STATE_COPY_DEST);

    D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC build{};
    build.Inputs = inputs;
    build.DestAccelerationStructureData = structure[0]->GetGPUVirtualAddress();
    build.ScratchAccelerationStructureData = scratch->GetGPUVirtualAddress();
    D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC report[2]{};
    for (int i = 0; i < 2; ++i) {
        report[i].DestBuffer = reports->GetGPUVirtualAddress() + 8 * i;
        report[i].InfoType = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_COMPACTED_SIZE;
    }
    Memory label(65536);
    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    auto* list = direct.list.Get();
    builder.entry<Builder::Build>(3)(builder.object, list, &build, 1, &report[0]);
    unordered_barrier(list);
    builder.entry<Builder::CopyStructure>(5)(builder.object, list, structure[1]->GetGPUVirtualAddress(),
                                             structure[0]->GetGPUVirtualAddress(),
                                             D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_CLONE);
    unordered_barrier(list);
    // The size of the built structure and of its copy, which a missing copy
    // leaves empty.
    D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC current{};
    current.DestBuffer = reports->GetGPUVirtualAddress() + 16;
    current.InfoType = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_CURRENT_SIZE;
    const D3D12_GPU_VIRTUAL_ADDRESS measured[] = {structure[0]->GetGPUVirtualAddress(),
                                                  structure[1]->GetGPUVirtualAddress()};
    method<EmitSizes>(list, slot_emit_sizes)(list, &current, 2, measured);
    unordered_barrier(list);
    Builder::Entry entry{};
    entry.desc = build;
    entry.desc.DestAccelerationStructureData = structure[2]->GetGPUVirtualAddress();
    entry.report_count = 1;
    entry.reports = &report[1];
    builder.entry<Builder::Batch>(4)(builder.object, list, 1, &entry);
    unordered_barrier(list);
    compute_fixture::transition(list, reports.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                D3D12_RESOURCE_STATE_COPY_SOURCE);
    list->CopyBufferRegion(readback.Get(), 0, reports.Get(), 0, 32);
    direct.write64(label.address(), 21);
    direct.submit();
    direct.finish("acceleration structure builds and copy");
    expect_value(label, 0, 21, "write after the builds");

    D3D12_RANGE range{0, 32}, empty{};
    UINT64* mapped{};
    check(readback->Map(0, &range, reinterpret_cast<void**>(&mapped)), "map size reports");
    const UINT64 single = mapped[0], batched = mapped[1], original = mapped[2], copied = mapped[3];
    readback->Unmap(0, &empty);
    if (!single || single > size.ResultDataMaxSizeInBytes)
        throw std::runtime_error("the single build reported no compacted size");
    if (batched != single) throw std::runtime_error("the batched build was not recorded like the single one");
    if (!original || copied != original) throw std::runtime_error("the acceleration structure was not copied");
}

// A console title can build into memory it allocated itself, with no resource
// naming the range. Here one allocation holds the vertices, the instance
// record, the scratch, a bottom-level structure and a scene that uses it. Size
// reports show that both builds and a copy of the bottom level ran.
void ray_tracing_guest_memory(ID3D12Device* device) {
    Builder builder(device);
    const float triangle[] = {0.f, 0.f, 0.f, 1.f, 0.f, 0.f, 0.f, 1.f, 0.f};
    D3D12_RAYTRACING_GEOMETRY_DESC geometry{};
    geometry.Type = D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES;
    geometry.Flags = D3D12_RAYTRACING_GEOMETRY_FLAG_OPAQUE;
    geometry.Triangles.VertexFormat = DXGI_FORMAT_R32G32B32_FLOAT;
    geometry.Triangles.VertexCount = 3;
    D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS bottom{};
    bottom.Type = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL;
    bottom.Flags = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_ALLOW_COMPACTION;
    bottom.NumDescs = 1;
    bottom.pGeometryDescs = &geometry;
    D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS top{};
    top.Type = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL;
    top.NumDescs = 1;
    top.DescsLayout = D3D12_ELEMENTS_LAYOUT_ARRAY;
    D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO bottom_size{}, top_size{};
    builder.entry<Builder::Prebuild>(7)(builder.object, &bottom, nullptr, &bottom_size);
    builder.entry<Builder::Prebuild>(7)(builder.object, &top, nullptr, &top_size);
    if (!bottom_size.ResultDataMaxSizeInBytes || !top_size.ResultDataMaxSizeInBytes)
        throw std::runtime_error("no prebuild sizes; ray tracing is unavailable");

    // Vertices and the instance record share the first 64 KiB, as small
    // console buffers do; scratch and both structures follow.
    auto round = [](UINT64 bytes, UINT64 unit) { return (bytes + unit - 1) / unit * unit; };
    const UINT64 instance_offset = 256;
    const UINT64 scratch_offset = 65536;
    const UINT64 scratch_bytes = std::max(bottom_size.ScratchDataSizeInBytes, top_size.ScratchDataSizeInBytes);
    const UINT64 bottom_offset = round(scratch_offset + scratch_bytes, 65536);
    const UINT64 top_offset = round(bottom_offset + bottom_size.ResultDataMaxSizeInBytes,
                                    D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BYTE_ALIGNMENT);
    const UINT64 total = round(top_offset + top_size.ResultDataMaxSizeInBytes, 65536);
    auto* memory = static_cast<BYTE*>(VirtualAlloc(nullptr, total, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE));
    if (!memory) throw std::runtime_error("allocate title memory");
    struct Release {
        void* memory;
        ~Release() { VirtualFree(memory, 0, MEM_RELEASE); }
    } release{memory};
    const UINT64 base = reinterpret_cast<UINT64>(memory);
    std::memcpy(memory, triangle, sizeof(triangle));
    geometry.Triangles.VertexBuffer = {base, 3 * sizeof(float)};
    D3D12_RAYTRACING_INSTANCE_DESC instance{};
    instance.Transform[0][0] = instance.Transform[1][1] = instance.Transform[2][2] = 1.f;
    instance.InstanceMask = 0xff;
    instance.AccelerationStructure = base + bottom_offset;
    std::memcpy(memory + instance_offset, &instance, sizeof(instance));
    top.InstanceDescs = base + instance_offset;

    auto gpu_buffer = [&](UINT64 bytes, D3D12_RESOURCE_STATES state) {
        return buffer(device, D3D12_HEAP_TYPE_DEFAULT, bytes, state,
                      D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
    };
    auto copy = gpu_buffer(bottom_size.ResultDataMaxSizeInBytes,
                           D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE);
    auto reports = gpu_buffer(32, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, 32, D3D12_RESOURCE_STATE_COPY_DEST);

    D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC build{};
    build.Inputs = bottom;
    build.DestAccelerationStructureData = base + bottom_offset;
    build.ScratchAccelerationStructureData = base + scratch_offset;
    D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC compacted{};
    compacted.DestBuffer = reports->GetGPUVirtualAddress();
    compacted.InfoType = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_COMPACTED_SIZE;
    Memory label(65536);
    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    auto* list = direct.list.Get();
    builder.entry<Builder::Build>(3)(builder.object, list, &build, 1, &compacted);
    unordered_barrier(list);
    builder.entry<Builder::CopyStructure>(5)(builder.object, list, copy->GetGPUVirtualAddress(),
                                             base + bottom_offset,
                                             D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_CLONE);
    D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC scene{};
    scene.Inputs = top;
    scene.DestAccelerationStructureData = base + top_offset;
    scene.ScratchAccelerationStructureData = base + scratch_offset;
    builder.entry<Builder::Build>(3)(builder.object, list, &scene, 0, nullptr);
    unordered_barrier(list);
    // Current sizes of the guest structure, its copy and the scene; a build
    // that never ran leaves its report empty.
    D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC current{};
    current.DestBuffer = reports->GetGPUVirtualAddress() + 8;
    current.InfoType = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_CURRENT_SIZE;
    const D3D12_GPU_VIRTUAL_ADDRESS measured[] = {base + bottom_offset, copy->GetGPUVirtualAddress(),
                                                  base + top_offset};
    method<EmitSizes>(list, slot_emit_sizes)(list, &current, 3, measured);
    unordered_barrier(list);
    compute_fixture::transition(list, reports.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                D3D12_RESOURCE_STATE_COPY_SOURCE);
    list->CopyBufferRegion(readback.Get(), 0, reports.Get(), 0, 32);
    direct.write64(label.address(), 22);
    direct.submit();
    direct.finish("builds in title memory");
    expect_value(label, 0, 22, "write after the builds in title memory");

    D3D12_RANGE range{0, 32}, empty{};
    UINT64* mapped{};
    check(readback->Map(0, &range, reinterpret_cast<void**>(&mapped)), "map size reports");
    const UINT64 packed = mapped[0], original = mapped[1], copied = mapped[2], placed = mapped[3];
    readback->Unmap(0, &empty);
    const auto sizes = " (compacted " + std::to_string(packed) + ", current " + std::to_string(original) +
                       ", copy " + std::to_string(copied) + ", scene " + std::to_string(placed) + ")";
    if (!packed || packed > bottom_size.ResultDataMaxSizeInBytes)
        throw std::runtime_error("the build in title memory reported no compacted size" + sizes);
    if (!original) throw std::runtime_error("the structure in title memory has no current size" + sizes);
    if (copied != original) throw std::runtime_error("the structure in title memory was not copied" + sizes);
    // RADV reports a scene's current size above its prebuild maximum.
    if (!placed) throw std::runtime_error("the scene in title memory was not built" + sizes);
}

// The console's quantized positions: a 32-byte header just before the first
// record gives the origin, the axis order and the scale, and each 8-byte
// record packs three coordinates of 24, 24 and 16 bits.
void packed_positions(BYTE* first, UINT order, const float (*positions)[3], size_t count,
                      float scale, const float origin[3]) {
    float header[8]{origin[0], origin[1], origin[2], 0.f, scale, scale, scale, 0.f};
    std::memcpy(&header[3], &order, sizeof(order));
    std::memcpy(first - sizeof(header), header, sizeof(header));
    for (size_t i = 0; i < count; ++i) {
        UINT q[3];
        for (int axis = 0; axis < 3; ++axis) {
            // Order 0 stores (z, y, x), anything above 1 stores (x, y, z).
            const int from = order == 0 ? 2 - axis : axis;
            q[axis] = static_cast<UINT>((positions[i][from] - origin[axis]) / scale + 0.5f);
        }
        const UINT words[2]{q[0] << 8 | (q[1] & 0xff), q[2] << 16 | (q[1] >> 8)};
        std::memcpy(first + 8 * i, words, sizeof(words));
    }
}

// The console's index format 53: a header, one record per meshlet naming its
// list of vertex indices, one word per triangle naming a meshlet and three
// entries of its list, then the lists. Each meshlet here lists three vertices.
void meshlet_indices(BYTE* stream, const std::vector<std::array<UINT16, 3>>& meshlets,
                     const std::vector<UINT>& triangle_meshlets) {
    const UINT count = static_cast<UINT>(meshlets.size());
    std::vector<UINT> words{0x80000000u | count, static_cast<UINT>(triangle_meshlets.size()), 0};
    for (UINT meshlet = 0; meshlet < count; ++meshlet) words.insert(words.end(), {0u, 3 * meshlet, 0u, 0u});
    for (UINT meshlet : triangle_meshlets) words.push_back(meshlet << 21 | 2u << 14 | 1u << 7 | 0u);
    std::memcpy(stream, words.data(), words.size() * sizeof(UINT));
    stream += words.size() * sizeof(UINT);
    for (const auto& list : meshlets) {
        std::memcpy(stream, list.data(), sizeof(list));
        stream += sizeof(list);
    }
}

// Title memory as a console title maps it: the same physical pages at one
// address for its CPU and, through the queue, at another for the GPU. The
// provider gives the GPU's address a copy of the pages as they were when it
// was mapped, so bytes the CPU writes afterwards are not seen there.
struct TwoViews {
    static constexpr SIZE_T pages = 2, page = 65536;
    UINT64 tokens[pages]{};
    BYTE* base;  // where the CPU writes
    BYTE* gpu;   // what the title names in commands
    explicit TwoViews(ID3D12CommandQueue* queue) {
        SIZE_T count = pages;
        if (!XMemAllocatePhysicalPages(0x20400000, &count, tokens) || count != pages)
            throw std::runtime_error("allocate physical pages");
        base = static_cast<BYTE*>(XMemVirtualAlloc(nullptr, pages * page, 0x20402000, 0x23, 0x2404));
        gpu = static_cast<BYTE*>(XMemVirtualAlloc(nullptr, pages * page, 0x20402000, 0x23, 0x2404));
        if (!base || !gpu || XMemMapPhysicalPages(base, pages, tokens) != base)
            throw std::runtime_error("map physical pages for the CPU");
        struct Range {
            UINT kind, source, count;
        } range{0, 0, static_cast<UINT>(pages)};
        struct Batch {
            UINT64 destination;
            UINT count;
            const Range* ranges;
        } batch{reinterpret_cast<UINT64>(gpu), 1, &range};
        using CopyMappings = HRESULT (WINAPI*)(ID3D12CommandQueue*, UINT, const Batch*, UINT64, UINT, UINT);
        check(method<CopyMappings>(queue, queue_copy_mappings)(queue, 1, &batch, reinterpret_cast<UINT64>(base),
                                                               static_cast<UINT>(pages), 0),
              "map physical pages for the GPU");
    }
    TwoViews(const TwoViews&) = delete;
    TwoViews& operator=(const TwoViews&) = delete;
    UINT64 address(size_t offset) const { return reinterpret_cast<UINT64>(gpu + offset); }
};

// A title streams bottom-level structures in the console's own form and asks
// the builder to rehydrate them in place from the geometry they were made
// from. Here two such structures sit 256 bytes apart in title memory, and are
// rehydrated in separate submissions. Their geometry is in the console's
// formats: meshlet-coded indices and quantized positions, with a transform on
// one geometry, and the second structure's geometry is only written after its
// call is recorded. The title names everything by the address it mapped for
// the GPU, which was mapped before any of it was written. A scene of both is
// traced, and each ray must report the instance, geometry and primitive the
// console's structure would.
void ray_tracing_rehydrate(ID3D12Device* device) {
    Builder builder(device);
    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    TwoViews memory(direct.queue.Get());
    constexpr size_t first_positions = 0x100, first_indices = 0x400, second_positions = 0x700,
                     transform_at = 0x900, late_positions = 0xb00, late_indices = 0xc00,
                     structure_at = 0x10100;
    const float zero[3]{};
    // Two triangles, listed by their meshlets in the opposite order.
    const float first[6][3]{{0, 0, 0}, {1, 0, 0}, {0, 1, 0}, {2, 0, 0}, {3, 0, 0}, {2, 1, 0}};
    packed_positions(memory.base + first_positions, 2, first, 6, 0.25f, zero);
    meshlet_indices(memory.base + first_indices, {{3, 4, 5}, {0, 1, 2}}, {1, 0});
    // One triangle without indices, moved one unit away by its transform.
    const float second[3][3]{{4, 0, 0}, {5, 0, 0}, {4, 1, 0}};
    packed_positions(memory.base + second_positions, 0, second, 3, 0.25f, zero);
    const float moved[12]{1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, -1};
    std::memcpy(memory.base + transform_at, moved, sizeof(moved));

    D3D12_RAYTRACING_GEOMETRY_DESC geometry[3]{};
    for (auto& item : geometry) {
        item.Type = D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES;
        item.Flags = D3D12_RAYTRACING_GEOMETRY_FLAG_OPAQUE;
        item.Triangles.VertexFormat = DXGI_FORMAT_R32G32_FLOAT;
    }
    geometry[0].Triangles.IndexFormat = static_cast<DXGI_FORMAT>(53);
    geometry[0].Triangles.IndexCount = 6;
    geometry[0].Triangles.IndexBuffer = memory.address(first_indices);
    geometry[0].Triangles.VertexCount = 6;
    geometry[0].Triangles.VertexBuffer = {memory.address(first_positions), 8};
    geometry[1].Triangles.VertexCount = 3;
    geometry[1].Triangles.VertexBuffer = {memory.address(second_positions), 8};
    geometry[1].Triangles.Transform3x4 = memory.address(transform_at);
    geometry[2].Triangles.IndexFormat = static_cast<DXGI_FORMAT>(53);
    geometry[2].Triangles.IndexCount = 3;
    geometry[2].Triangles.IndexBuffer = memory.address(late_indices);
    geometry[2].Triangles.VertexCount = 3;
    geometry[2].Triangles.VertexBuffer = {memory.address(late_positions), 8};

    // Each structure begins with the console's header: flags, a count of
    // 64-byte nodes, a checksum and an element count.
    const UINT64 structures[2]{memory.address(structure_at), memory.address(structure_at + 256)};
    const UINT header[2][4]{{0x81020006u, 3, 0, 3}, {0x81020006u, 1, 0, 1}};
    D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC rehydrate[2]{};
    for (int i = 0; i < 2; ++i) {
        std::memcpy(memory.base + structure_at + 256 * i, header[i], sizeof(header[i]));
        rehydrate[i].Inputs.Type = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL;
        rehydrate[i].Inputs.Flags = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PREFER_FAST_TRACE |
                                    D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_MINIMIZE_MEMORY;
        rehydrate[i].DestAccelerationStructureData = rehydrate[i].SourceAccelerationStructureData = structures[i];
    }
    rehydrate[0].Inputs.NumDescs = 2;
    rehydrate[0].Inputs.pGeometryDescs = &geometry[0];
    rehydrate[1].Inputs.NumDescs = 1;
    rehydrate[1].Inputs.pGeometryDescs = &geometry[2];

    D3D12_RAYTRACING_INSTANCE_DESC instances[2]{};
    for (int i = 0; i < 2; ++i) {
        instances[i].Transform[0][0] = instances[i].Transform[1][1] = instances[i].Transform[2][2] = 1.f;
        instances[i].InstanceMask = 0xff;
        instances[i].AccelerationStructure = structures[i];
    }
    instances[0].InstanceID = 7;
    instances[1].InstanceID = 9;
    instances[1].Transform[1][3] = 10.f;
    auto records = buffer(device, D3D12_HEAP_TYPE_UPLOAD, sizeof(instances), D3D12_RESOURCE_STATE_GENERIC_READ);
    compute_fixture::upload(records.Get(), instances, sizeof(instances));
    D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS top{};
    top.Type = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL;
    top.NumDescs = 2;
    top.DescsLayout = D3D12_ELEMENTS_LAYOUT_ARRAY;
    top.InstanceDescs = records->GetGPUVirtualAddress();
    D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO top_size{};
    builder.entry<Builder::Prebuild>(7)(builder.object, &top, nullptr, &top_size);
    if (!top_size.ResultDataMaxSizeInBytes) throw std::runtime_error("no prebuild sizes; ray tracing is unavailable");
    auto gpu_buffer = [&](UINT64 bytes, D3D12_RESOURCE_STATES state) {
        return buffer(device, D3D12_HEAP_TYPE_DEFAULT, bytes, state, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
    };
    auto scene = gpu_buffer(top_size.ResultDataMaxSizeInBytes, D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE);
    auto scratch = gpu_buffer(top_size.ScratchDataSizeInBytes, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);

    struct Probe {
        float origin[3], minimum, direction[3], maximum;
        UINT flags;
    };
    static_assert(sizeof(Probe) == 36);
    struct Hit {
        UINT instance, geometry, primitive;
        float distance;
    };
    // Rays down the z axis from z = 5 and what each must report.
    const struct {
        float x, y;
        Hit hit;
    } cases[]{
        {0.25f, 0.25f, {7, 0, 0, 5.f}},    // the first triangle its index stream lists
        {2.25f, 0.25f, {7, 0, 1, 5.f}},    // the second, from the stream's first meshlet
        {4.25f, 0.25f, {7, 1, 0, 6.f}},    // the second geometry, behind by its transform
        {0.25f, 10.25f, {9, 0, 0, 7.f}},   // the neighbouring structure
        {20.f, 20.f, {~0u, ~0u, ~0u, 0.f}},
    };
    constexpr UINT rays = static_cast<UINT>(std::size(cases));
    Probe probes[rays]{};
    for (UINT i = 0; i < rays; ++i) probes[i] = {{cases[i].x, cases[i].y, 5.f}, 0.f, {0.f, 0.f, -1.f}, 100.f, 0};
    auto probe_buffer = buffer(device, D3D12_HEAP_TYPE_UPLOAD, sizeof(probes), D3D12_RESOURCE_STATE_GENERIC_READ);
    compute_fixture::upload(probe_buffer.Get(), probes, sizeof(probes));
    constexpr UINT64 result_bytes = rays * sizeof(Hit);
    auto results = gpu_buffer(result_bytes, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, result_bytes, D3D12_RESOURCE_STATE_COPY_DEST);

    D3D12_ROOT_PARAMETER params[3]{};
    params[0].ParameterType = D3D12_ROOT_PARAMETER_TYPE_SRV;
    params[1].ParameterType = D3D12_ROOT_PARAMETER_TYPE_UAV;
    params[2].ParameterType = D3D12_ROOT_PARAMETER_TYPE_SRV;
    params[2].Descriptor.ShaderRegister = 1;
    D3D12_ROOT_SIGNATURE_DESC signature{3, params, 0, nullptr, D3D12_ROOT_SIGNATURE_FLAG_NONE};
    ComPtr<ID3DBlob> serialized;
    check(D3D12SerializeRootSignature(&signature, D3D_ROOT_SIGNATURE_VERSION_1, &serialized, nullptr),
          "serialize ray query root signature");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(),
                                      IID_PPV_ARGS(&root)), "create ray query root signature");
    D3D12_COMPUTE_PIPELINE_STATE_DESC shader{};
    shader.pRootSignature = root.Get();
    shader.CS = {ray_query_dxil, sizeof(ray_query_dxil)};
    ComPtr<ID3D12PipelineState> pipeline;
    check(method<CreateComputePipeline>(device, device_create_compute)(device, &shader, 0, nullptr,
                                                                       IID_PPV_ARGS(&pipeline)),
          "create ray query pipeline");

    Memory label(65536);
    auto* list = direct.list.Get();
    using Rehydrate = HRESULT (WINAPI*)(void*, ID3D12GraphicsCommandList*, UINT,
                                        const D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC*, const UINT64*);
    // The neighbours arrive in separate submissions, as streamed structures
    // do: the second must not count as written over the first.
    // The call's second array names each structure where the CPU sees it.
    const UINT64 cpu_view[2]{reinterpret_cast<UINT64>(memory.base + structure_at),
                             reinterpret_cast<UINT64>(memory.base + structure_at + 256)};
    check(builder.entry<Rehydrate>(8)(builder.object, list, 1, &rehydrate[0], &cpu_view[0]),
          "rehydrate the first structure");
    direct.write64(label.address(), 22);
    direct.submit();
    direct.finish("the first rehydrated structure");
    expect_value(label, 0, 22, "write after the first rehydrated structure");
    direct.reopen();
    check(builder.entry<Rehydrate>(8)(builder.object, list, 1, &rehydrate[1], &cpu_view[1]),
          "rehydrate the second structure");
    // The second structure's geometry arrives after its call is recorded.
    const float late[3][3]{{0, 0, -2}, {1, 0, -2}, {0, 1, -2}};
    const float late_origin[3]{0, 0, -2};
    packed_positions(memory.base + late_positions, 2, late, 3, 0.25f, late_origin);
    meshlet_indices(memory.base + late_indices, {{0, 1, 2}}, {0});
    D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC build{};
    build.Inputs = top;
    build.DestAccelerationStructureData = scene->GetGPUVirtualAddress();
    build.ScratchAccelerationStructureData = scratch->GetGPUVirtualAddress();
    builder.entry<Builder::Build>(3)(builder.object, list, &build, 0, nullptr);
    unordered_barrier(list);
    method<SetRoot>(list, slot_compute_root)(list, root.Get());
    list->SetPipelineState(pipeline.Get());
    method<SetUav>(list, slot_root_srv)(list, 0, scene->GetGPUVirtualAddress());
    method<SetUav>(list, slot_root_uav)(list, 1, results->GetGPUVirtualAddress());
    method<SetUav>(list, slot_root_srv)(list, 2, probe_buffer->GetGPUVirtualAddress());
    list->Dispatch(rays, 1, 1);
    compute_fixture::transition(list, results.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                D3D12_RESOURCE_STATE_COPY_SOURCE);
    list->CopyBufferRegion(readback.Get(), 0, results.Get(), 0, result_bytes);
    direct.write64(label.address(), 23);
    direct.submit();
    direct.finish("rehydrated structures and their scene");
    expect_value(label, 0, 23, "write after the rehydrated scene");

    Hit found[rays]{};
    D3D12_RANGE range{0, static_cast<SIZE_T>(result_bytes)}, empty{};
    void* mapped{};
    check(readback->Map(0, &range, &mapped), "map ray results");
    std::memcpy(found, mapped, result_bytes);
    readback->Unmap(0, &empty);
    for (UINT i = 0; i < rays; ++i) {
        const Hit& expected = cases[i].hit;
        if (found[i].instance != expected.instance || found[i].geometry != expected.geometry ||
            found[i].primitive != expected.primitive || std::fabs(found[i].distance - expected.distance) > 1e-3f) {
            char message[200];
            std::snprintf(message, sizeof(message),
                          "ray %u: expected instance %d geometry %d primitive %d at %.3f, found %d %d %d at %.3f",
                          i, int(expected.instance), int(expected.geometry), int(expected.primitive),
                          expected.distance, int(found[i].instance), int(found[i].geometry),
                          int(found[i].primitive), found[i].distance);
            throw std::runtime_error(message);
        }
    }
}

// Slot 80 (dispatch_flagged) with USE_THREAD_DIMENSIONS (0x20) counts threads
// rather than groups, converting via the bound PSO's [numthreads(8, 1, 1)].
void thread_dimensions(ID3D12Device* device) {
    D3D12_ROOT_PARAMETER params[2]{};
    params[0].ParameterType = D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS;
    params[0].Constants = {1, 2, 2};
    params[1].ParameterType = D3D12_ROOT_PARAMETER_TYPE_UAV;
    params[1].Descriptor = {2, 1};
    D3D12_ROOT_SIGNATURE_DESC sig{};
    sig.NumParameters = 2;
    sig.pParameters = params;
    ComPtr<ID3DBlob> serialized;
    check(D3D12SerializeRootSignature(&sig, D3D_ROOT_SIGNATURE_VERSION_1, &serialized, nullptr),
          "serialize thread-dimensions root signature");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(),
                                      IID_PPV_ARGS(&root)), "create thread-dimensions root signature");
    D3D12_COMPUTE_PIPELINE_STATE_DESC shader{};
    shader.pRootSignature = root.Get();
    shader.CS = {course_dxil, sizeof(course_dxil)};
    ComPtr<ID3D12PipelineState> pipeline, stream_pipeline;
    check(method<CreateComputePipeline>(device, device_create_compute)(device, &shader, 0, nullptr,
                                                                       IID_PPV_ARGS(&pipeline)),
          "create thread-dimensions compute pipeline");
    struct alignas(void*) ComputeStream {
        int root_tag = 0;
        UINT pad0 = 0;
        ID3D12RootSignature* root = nullptr;
        int cs_tag = 6;
        UINT pad1 = 0;
        D3D12_SHADER_BYTECODE cs{};
    } stream{};
    static_assert(sizeof(ComputeStream) == 40);
    stream.root = root.Get();
    stream.cs = shader.CS;
    D3D12_PIPELINE_STATE_STREAM_DESC stream_desc{sizeof(stream), &stream};
    check(method<CreateStreamPipeline>(device, device_create_stream)(device, &stream_desc,
                                                                     IID_PPV_ARGS(&stream_pipeline)),
          "create thread-dimensions stream pipeline");

    Queue compute(device, D3D12_COMMAND_LIST_TYPE_COMPUTE);
    auto dispatch_flagged = method<DispatchFlagged>(compute.list.Get(), slot_dispatch_flagged);

    // Fail closed when flags & 0x20 is set without a bound compute pipeline.
    dispatch_flagged(compute.list.Get(), 8, 1, 1, 0x20);
    if (SUCCEEDED(compute.list->Close()))
        throw std::runtime_error("slot 80 thread-dimension dispatch accepted without a pipeline");
    check(compute.list->Reset(compute.allocator.Get(), pipeline.Get()), "reset after missing pipeline");
    compute.list->ClearState(nullptr);
    dispatch_flagged(compute.list.Get(), 8, 1, 1, 0x8020);
    if (SUCCEEDED(compute.list->Close()))
        throw std::runtime_error("slot 80 thread-dimension dispatch accepted after ClearState(nullptr)");

    constexpr UINT words = 64, guard = 0xdeadbeefu;
    constexpr UINT64 bytes = words * sizeof(UINT);
    std::array<UINT, words> seed_words;
    seed_words.fill(guard);
    auto seed = buffer(device, D3D12_HEAP_TYPE_UPLOAD, bytes, D3D12_RESOURCE_STATE_GENERIC_READ);
    auto output = buffer(device, D3D12_HEAP_TYPE_DEFAULT, bytes, D3D12_RESOURCE_STATE_COPY_DEST,
                         D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
    auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, bytes, D3D12_RESOURCE_STATE_COPY_DEST);
    compute_fixture::upload(seed.Get(), seed_words.data(), bytes);

    struct Pass {
        UINT x;
        UINT flags;
        UINT row_count;
        UINT expected_written;
        UINT mode; // 0 = Reset(pipeline), 1 = SetPipelineState(stream_pipeline), 2 = ClearState(pipeline)
    };
    constexpr Pass passes[] = {
        {16, 0x20, words, 16, 0},
        {8, 0x8020, words, 8, 1},
        {70000, 0x20, 24, 24, 2},
        {13, 0x20, 13, 13, 0},
        {0, 0x20, words, 0, 0},
        {2, 0x0, words, 16, 0},
        {1, 0x8000, words, 8, 1},
    };
    const UINT seed_value = 7;
    auto expected_word = [&](UINT row) {
        const UINT beat = (row + seed_value) & 3u;
        const UINT coin = beat == 3u ? 1u : beat;
        UINT mixed = (row + seed_value) * 747796405u + 2891336453u;
        mixed ^= mixed >> 16;
        const UINT barrier = (coin + 1u + (mixed & 1u)) % 3u;
        return coin | (barrier << 8);
    };
    for (size_t i = 0; i < std::size(passes); ++i) {
        const auto& pass = passes[i];
        check(compute.allocator->Reset(), "reset thread-dimensions allocator");
        check(compute.list->Reset(compute.allocator.Get(), pass.mode == 0 ? pipeline.Get() : nullptr),
              "reset thread-dimensions list");
        if (i != 0)
            compute_fixture::transition(compute.list.Get(), output.Get(),
                                        D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST);
        compute.list->CopyBufferRegion(output.Get(), 0, seed.Get(), 0, bytes);
        compute_fixture::transition(compute.list.Get(), output.Get(),
                                    D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
        if (pass.mode == 1)
            compute.list->SetPipelineState(stream_pipeline.Get());
        else if (pass.mode == 2)
            compute.list->ClearState(pipeline.Get());
        auto* raw = compute.list.Get();
        const UINT constants[] = {seed_value, pass.row_count};
        method<SetRoot>(raw, slot_compute_root)(raw, root.Get());
        method<SetConstants>(raw, slot_root_constants)(raw, 0, 2, constants, 0);
        method<SetUav>(raw, slot_root_uav)(raw, 1, output->GetGPUVirtualAddress());
        dispatch_flagged(raw, pass.x, 1, 1, pass.flags);
        compute_fixture::transition(compute.list.Get(), output.Get(),
                                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE);
        compute.list->CopyBufferRegion(readback.Get(), 0, output.Get(), 0, bytes);
        compute.submit();
        compute.finish("slot 80 dispatch");

        std::array<UINT, words> actual{};
        D3D12_RANGE range{0, static_cast<SIZE_T>(bytes)}, empty{};
        void* mapped{};
        check(readback->Map(0, &range, &mapped), "map thread-dimensions readback");
        std::memcpy(actual.data(), mapped, bytes);
        readback->Unmap(0, &empty);
        for (UINT row = 0; row < words; ++row) {
            const UINT expected = row < pass.expected_written ? expected_word(row) : guard;
            if (actual[row] != expected) {
                char msg[160];
                std::snprintf(msg, sizeof(msg),
                              "pass %zu (x=%u flags=%#x) row %u: expected %#x, found %#x",
                              i, pass.x, pass.flags, row, expected, actual[row]);
                throw std::runtime_error(msg);
            }
        }
    }
}

// Exercise shader-visible descriptor slot reclamation on an oversized paged heap:
// (1) sustained >2x turnover with mixed table lengths, (2) overlapping in-flight
// Reset() survival, (3) ClearState pin retention, (4) re-binding an evicted slot
// with fresh view data, and (5) fail-closed E_OUTOFMEMORY without corruption.
void descriptor_reclaim(ID3D12Device* device) {
    constexpr UINT block_16k = 16384;
    constexpr UINT block_8k = 8192;
    constexpr UINT num_16k_blocks = 128;
    constexpr UINT heap_descriptors = num_16k_blocks * block_16k; // 2,097,152 > 1,000,000 native limit
    constexpr UINT max_dispatches = 1024;
    constexpr UINT sentinel = 0xdeadbeefu;

    D3D12_DESCRIPTOR_HEAP_DESC heap_desc{
        D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, heap_descriptors, D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 1};
    ComPtr<ID3D12DescriptorHeap> view_heap;
    // The steps below count on a resident limit of 1,000,000 descriptors. The
    // provider takes the host's MaxViewDescriptorHeapSize when that is larger,
    // and then step 5 never overflows. Pin the limit while this heap is created.
    if (!SetEnvironmentVariableW(L"TUXBOX_DESCRIPTOR_HEAP_LIMIT", L"1000000"))
        throw std::runtime_error("cannot pin the descriptor heap limit");
    const HRESULT created = device->CreateDescriptorHeap(&heap_desc, IID_PPV_ARGS(&view_heap));
    SetEnvironmentVariableW(L"TUXBOX_DESCRIPTOR_HEAP_LIMIT", nullptr);
    check(created, "create oversized view heap");
    const UINT stride = device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
    if (!stride) throw std::runtime_error("zero view descriptor stride");
    const SIZE_T cpu_base = method<frame_abi::CpuHandle>(view_heap.Get(), 9)(view_heap.Get());
    const UINT64 gpu_base = method<GpuHandle>(view_heap.Get(), 10)(view_heap.Get());
    auto cpu_at = [&](UINT index) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        return {cpu_base + SIZE_T(index) * stride};
    };
    auto gpu_at = [&](UINT index) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        return {gpu_base + UINT64(index) * stride};
    };

    auto make_pipeline = [&](UINT table_len, ComPtr<ID3D12RootSignature>& rs, ComPtr<ID3D12PipelineState>& pso) {
        D3D12_DESCRIPTOR_RANGE ranges[3]{};
        ranges[0] = {D3D12_DESCRIPTOR_RANGE_TYPE_CBV, 1, 0, 0, 0};
        ranges[1] = {D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 1, 0, 0, 1};
        ranges[2] = {D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 1, 0, 0, table_len - 1};
        D3D12_ROOT_PARAMETER param{};
        param.ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
        param.DescriptorTable = {3, ranges};
        D3D12_ROOT_SIGNATURE_DESC sig{1, &param, 0, nullptr, D3D12_ROOT_SIGNATURE_FLAG_NONE};
        ComPtr<ID3DBlob> blob;
        check(D3D12SerializeRootSignature(&sig, D3D_ROOT_SIGNATURE_VERSION_1, &blob, nullptr),
              "serialize reclaim root signature");
        check(device->CreateRootSignature(0, blob->GetBufferPointer(), blob->GetBufferSize(), IID_PPV_ARGS(&rs)),
              "create reclaim root signature");
        D3D12_COMPUTE_PIPELINE_STATE_DESC pso_desc{};
        pso_desc.pRootSignature = rs.Get();
        pso_desc.CS = {reclaim_dxil, sizeof(reclaim_dxil)};
        check(method<CreateComputePipeline>(device, device_create_compute)(
                  device, &pso_desc, 0, nullptr, IID_PPV_ARGS(&pso)),
              "create reclaim compute pipeline");
    };
    ComPtr<ID3D12RootSignature> rs16, rs8;
    ComPtr<ID3D12PipelineState> pso16, pso8;
    make_pipeline(block_16k, rs16, pso16);
    make_pipeline(block_8k, rs8, pso8);

    auto cb_key = [](UINT k) { return 0x1000u + k * 7u + 3u; };
    auto cb_salt = [](UINT k) { return 0x50000u + k * 13u + 5u; };
    auto src_word = [](UINT k) { return 0x200u + k * 19u + 11u; };
    auto expected_word = [&](UINT k) { return src_word(k) * 131u + cb_key(k) * 17u + cb_salt(k); };

    std::vector<UINT> cb_bytes(size_t(max_dispatches) * 64, 0);
    std::vector<UINT> src_words(max_dispatches, 0);
    std::vector<UINT> seed_words(max_dispatches, sentinel);
    std::vector<UINT> expected_out(max_dispatches, sentinel);
    for (UINT k = 0; k < max_dispatches; ++k) {
        cb_bytes[size_t(k) * 64 + 0] = cb_key(k);
        cb_bytes[size_t(k) * 64 + 1] = cb_salt(k);
        src_words[k] = src_word(k);
    }
    const UINT64 out_bytes = UINT64(max_dispatches) * sizeof(UINT);
    auto cb_pool = buffer(device, D3D12_HEAP_TYPE_UPLOAD, UINT64(max_dispatches) * 256,
                          D3D12_RESOURCE_STATE_GENERIC_READ);
    auto src_pool = buffer(device, D3D12_HEAP_TYPE_UPLOAD, out_bytes, D3D12_RESOURCE_STATE_GENERIC_READ);
    auto seed_buf = buffer(device, D3D12_HEAP_TYPE_UPLOAD, out_bytes, D3D12_RESOURCE_STATE_GENERIC_READ);
    auto dst_buf = buffer(device, D3D12_HEAP_TYPE_DEFAULT, out_bytes, D3D12_RESOURCE_STATE_COPY_DEST,
                          D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
    auto readback_buf = buffer(device, D3D12_HEAP_TYPE_READBACK, out_bytes, D3D12_RESOURCE_STATE_COPY_DEST);
    compute_fixture::upload(cb_pool.Get(), cb_bytes.data(), cb_bytes.size() * sizeof(UINT));
    compute_fixture::upload(src_pool.Get(), src_words.data(), size_t(out_bytes));
    compute_fixture::upload(seed_buf.Get(), seed_words.data(), size_t(out_bytes));

    const UINT64 cb_va = cb_pool->GetGPUVirtualAddress();
    auto author_table = [&](UINT slot_base, UINT table_len, UINT data_idx, UINT out_idx) {
        D3D12_CONSTANT_BUFFER_VIEW_DESC cbv{cb_va + UINT64(data_idx) * 256, 256};
        device->CreateConstantBufferView(&cbv, cpu_at(slot_base + 0));
        D3D12_SHADER_RESOURCE_VIEW_DESC srv{};
        srv.ViewDimension = D3D12_SRV_DIMENSION_BUFFER;
        srv.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
        srv.Buffer.FirstElement = data_idx;
        srv.Buffer.NumElements = 1;
        srv.Buffer.StructureByteStride = sizeof(UINT);
        device->CreateShaderResourceView(src_pool.Get(), &srv, cpu_at(slot_base + 1));
        D3D12_UNORDERED_ACCESS_VIEW_DESC uav{};
        uav.ViewDimension = D3D12_UAV_DIMENSION_BUFFER;
        uav.Buffer.FirstElement = out_idx;
        uav.Buffer.NumElements = 1;
        uav.Buffer.StructureByteStride = sizeof(UINT);
        device->CreateUnorderedAccessView(dst_buf.Get(), nullptr, &uav, cpu_at(slot_base + table_len - 1));
        expected_out[out_idx] = expected_word(data_idx);
    };
    auto bind_and_dispatch = [&](ID3D12GraphicsCommandList* list, UINT slot_base, UINT table_len, bool set_pso = true) {
        ID3D12DescriptorHeap* heaps[]{view_heap.Get()};
        method<BindHeaps>(list, slot_bind_heaps)(list, 1, heaps);
        ID3D12RootSignature* rs = table_len == block_16k ? rs16.Get() : rs8.Get();
        ID3D12PipelineState* pso = table_len == block_16k ? pso16.Get() : pso8.Get();
        method<SetRoot>(list, slot_compute_root)(list, rs);
        if (set_pso) list->SetPipelineState(pso);
        method<SetTable>(list, slot_compute_table)(list, 0, gpu_at(slot_base));
        list->Dispatch(1, 1, 1);
    };

    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    Queue compute(device, D3D12_COMMAND_LIST_TYPE_COMPUTE);
    direct.list->CopyBufferRegion(dst_buf.Get(), 0, seed_buf.Get(), 0, out_bytes);
    compute_fixture::transition(direct.list.Get(), dst_buf.Get(),
                                D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    direct.submit();
    direct.finish("seed reclaim output buffer");

    auto verify_outputs = [&](UINT check_count, const char* phase) {
        direct.reopen();
        compute_fixture::transition(direct.list.Get(), dst_buf.Get(),
                                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE);
        direct.list->CopyBufferRegion(readback_buf.Get(), 0, dst_buf.Get(), 0, out_bytes);
        compute_fixture::transition(direct.list.Get(), dst_buf.Get(),
                                    D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
        direct.submit();
        direct.finish(phase);
        D3D12_RANGE range{0, SIZE_T(out_bytes)}, empty{};
        const UINT* mapped = nullptr;
        check(readback_buf->Map(0, &range, reinterpret_cast<void**>(const_cast<UINT**>(&mapped))),
              "map reclaim readback");
        for (UINT i = 0; i < check_count; ++i) {
            if (mapped[i] != expected_out[i]) {
                const UINT got = mapped[i];
                readback_buf->Unmap(0, &empty);
                char msg[192];
                std::snprintf(msg, sizeof(msg), "%s mismatch at output %u: expected 0x%08x, found 0x%08x",
                              phase, i, expected_out[i], got);
                throw std::runtime_error(msg);
            }
        }
        readback_buf->Unmap(0, &empty);
    };

    // 1. Sustained turnover (> 2x shader-visible limit) mixing 8K and 16K spans.
    //    Emit 32 8K tables (blocks 0..16) interleaved with 16K blocks 16..67,
    //    then 61 16K blocks 67..128 so all of 0..67 are evicted by the end.
    struct TableJob { UINT slot_base; UINT len; UINT idx; };
    std::vector<TableJob> jobs;
    jobs.reserve(144);
    UINT next_idx = 0;
    for (UINT i = 0; i < 32; ++i) {
        jobs.push_back({i * block_8k, block_8k, next_idx++});
        jobs.push_back({(16 + i) * block_16k, block_16k, next_idx++});
    }
    for (UINT b = 48; b < 128; ++b) {
        jobs.push_back({b * block_16k, block_16k, next_idx++});
    }
    for (const auto& job : jobs) author_table(job.slot_base, job.len, job.idx, job.idx);
    auto release_leases = [](Queue& q) {
        q.reopen();
        q.close();
    };
    for (size_t offset = 0; offset < jobs.size(); offset += 8) {
        if (offset > 0) compute.reopen();
        const size_t batch_end = std::min(offset + 8, jobs.size());
        for (size_t i = offset; i < batch_end; ++i)
            bind_and_dispatch(compute.list.Get(), jobs[i].slot_base, jobs[i].len);
        compute.submit();
        compute.finish("sustained descriptor turnover batch");
    }
    release_leases(compute);
    verify_outputs(next_idx, "sustained turnover");

    // 2. Overlapping / double-buffered in-flight safety across CPU Reset():
    //    L1 (20 tables) is queued behind gate_fence, L0 (30 tables) runs & finishes
    //    on direct (so L1's last_used is OLDER than L0's), L2 (15 tables) queues
    //    behind gate_fence, L1->Reset() drops L1's recording leases while L1 is
    //    still in-flight, and L3 (25 tables) forces eviction of L0 without touching L1/L2.
    const UINT sub2_base = next_idx;
    for (UINT b = 0; b < 90; ++b) {
        author_table(b * block_16k, block_16k, next_idx, next_idx);
        ++next_idx;
    }
    auto make_list = [&](D3D12_COMMAND_LIST_TYPE type, ComPtr<ID3D12CommandAllocator>& alloc,
                         ComPtr<ID3D12GraphicsCommandList>& list) {
        check(device->CreateCommandAllocator(type, IID_PPV_ARGS(&alloc)), "create extra allocator");
        const UINT list_desc[] = {static_cast<UINT>(type), 0, 1, 0};
        check(method<frame_abi::CreateList>(device, device_create_list)(
                  device, list_desc, alloc.Get(), nullptr, IID_PPV_ARGS(&list)),
              "create extra list");
    };
    {
        ComPtr<ID3D12Fence> gate_fence;
        check(device->CreateFence(0, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&gate_fence)), "create gate fence");
        check(method<WaitQueue>(compute.queue.Get(), queue_wait)(compute.queue.Get(), gate_fence.Get(), 1),
              "stall compute queue on gate fence");

        ComPtr<ID3D12CommandAllocator> alloc_l1, alloc_l2, alloc_l3;
        ComPtr<ID3D12GraphicsCommandList> list_l1, list_l2;
        make_list(D3D12_COMMAND_LIST_TYPE_COMPUTE, alloc_l1, list_l1);
        make_list(D3D12_COMMAND_LIST_TYPE_COMPUTE, alloc_l2, list_l2);
        check(device->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COMPUTE, IID_PPV_ARGS(&alloc_l3)),
              "create spare L3 allocator");

        check(list_l1->Reset(alloc_l1.Get(), nullptr), "open L1");
        for (UINT b = 0; b < 20; ++b) bind_and_dispatch(list_l1.Get(), b * block_16k, block_16k);
        check(list_l1->Close(), "close L1");
        {
            ID3D12CommandList* batch[]{list_l1.Get()};
            compute.queue->ExecuteCommandLists(1, batch);
        }

        direct.reopen();
        for (UINT b = 20; b < 50; ++b) bind_and_dispatch(direct.list.Get(), b * block_16k, block_16k);
        direct.submit();
        direct.finish("subtest 2 L0");
        release_leases(direct);

        check(list_l2->Reset(alloc_l2.Get(), nullptr), "open L2");
        for (UINT b = 50; b < 65; ++b) bind_and_dispatch(list_l2.Get(), b * block_16k, block_16k);
        check(list_l2->Close(), "close L2");
        {
            ID3D12CommandList* batch[]{list_l2.Get()};
            compute.queue->ExecuteCommandLists(1, batch);
        }

        // Reset L1 on the CPU while L1 is still in-flight behind gate_fence!
        check(list_l1->Reset(alloc_l3.Get(), nullptr), "reset L1 while in-flight");
        for (UINT b = 65; b < 90; ++b) bind_and_dispatch(list_l1.Get(), b * block_16k, block_16k);
        check(list_l1->Close(), "close L3");
        {
            ID3D12CommandList* batch[]{list_l1.Get()};
            compute.queue->ExecuteCommandLists(1, batch);
        }
        check(gate_fence->Signal(1), "release gate fence");
        compute.signal();
        compute.finish("overlapping L1/L2/L3 execution");
    }
    verify_outputs(sub2_base + 90, "overlapping in-flight reset");

    // 3. ClearState pin retention:
    //    Fill 0..61 on direct, open CL and bind+dispatch table A (block 61),
    //    call ClearState, refresh 1..61 on direct (making A oldest), then bind
    //    60 new tables 62..122 (ending with table B at block 121) inside CL.
    for (UINT b = 0; b < 122; ++b) {
        author_table(b * block_16k, block_16k, next_idx, next_idx);
        ++next_idx;
    }
    direct.reopen();
    for (UINT b = 0; b < 61; ++b) bind_and_dispatch(direct.list.Get(), b * block_16k, block_16k);
    direct.submit();
    direct.finish("subtest 3 fill");
    release_leases(direct);

    compute.reopen();
    bind_and_dispatch(compute.list.Get(), 61u * block_16k, block_16k); // table A (block 61)
    compute.list->ClearState(pso16.Get());

    direct.reopen();
    for (UINT b = 1; b < 61; ++b) bind_and_dispatch(direct.list.Get(), b * block_16k, block_16k);
    direct.submit();
    direct.finish("subtest 3 refresh 1..61");
    release_leases(direct);

    // Bind 60 new tables 62..122 after ClearState; keep pso16 from ClearState(pso16).
    for (UINT b = 62; b < 122; ++b)
        bind_and_dispatch(compute.list.Get(), b * block_16k, block_16k, false);
    compute.submit();
    compute.finish("subtest 3 ClearState CL");
    verify_outputs(next_idx, "ClearState pin retention");

    // 4. Re-bind after eviction with fresh view data at logical block 0 (evicted in step 3).
    const UINT fresh_out = next_idx++;
    author_table(0, block_16k, fresh_out, fresh_out);
    compute.reopen();
    bind_and_dispatch(compute.list.Get(), 0, block_16k);
    compute.submit();
    compute.finish("re-bind evicted slot 0");
    release_leases(compute);
    verify_outputs(next_idx, "re-bind evicted slot");

    // 5. Fail-closed OOM control:
    //    Pin block 0 in open list L_pinned, then pin 62 more unique tables (total 63 * 16K
    //    = 1,032,192 > 1,000,000 limit) in open list L_oom and confirm Close() returns
    //    E_OUTOFMEMORY without corrupting L_pinned or preventing post-Reset recovery.
    const UINT pinned_out = next_idx++;
    const UINT recovery_out = next_idx++;
    author_table(0, block_16k, pinned_out, pinned_out);
    author_table(1 * block_16k, block_16k, recovery_out, recovery_out);

    ComPtr<ID3D12CommandAllocator> alloc_pin, alloc_oom;
    ComPtr<ID3D12GraphicsCommandList> list_pin, list_oom;
    make_list(D3D12_COMMAND_LIST_TYPE_COMPUTE, alloc_pin, list_pin);
    make_list(D3D12_COMMAND_LIST_TYPE_COMPUTE, alloc_oom, list_oom);

    check(list_pin->Reset(alloc_pin.Get(), nullptr), "open pinned control list");
    bind_and_dispatch(list_pin.Get(), 0, block_16k);

    check(list_oom->Reset(alloc_oom.Get(), nullptr), "open OOM control list");
    for (UINT b = 1; b <= 62; ++b) bind_and_dispatch(list_oom.Get(), b * block_16k, block_16k);
    const HRESULT oom_hr = list_oom->Close();
    if (oom_hr != E_OUTOFMEMORY) {
        char msg[128];
        std::snprintf(msg, sizeof(msg), "expected E_OUTOFMEMORY on unsubmitted pin overflow, got 0x%08lx",
                      static_cast<unsigned long>(oom_hr));
        throw std::runtime_error(msg);
    }

    check(list_pin->Close(), "close pinned control list after OOM");
    {
        ID3D12CommandList* batch[]{list_pin.Get()};
        compute.queue->ExecuteCommandLists(1, batch);
    }
    check(list_oom->Reset(alloc_oom.Get(), nullptr), "reset failed OOM list");
    bind_and_dispatch(list_oom.Get(), 1 * block_16k, block_16k);
    check(list_oom->Close(), "close recovery list");
    {
        ID3D12CommandList* batch[]{list_oom.Get()};
        compute.queue->ExecuteCommandLists(1, batch);
    }
    compute.signal();
    compute.finish("pinned control and post-OOM recovery");
    verify_outputs(next_idx, "fail-closed OOM control");
}

// A title allocator that hands back memory it has used before, as a title's own
// allocator may. The provider takes a descriptor heap's records from it.
UINT reused_allocations = 0;
void* WINAPI reused_title_memory(SIZE_T bytes, UINT64 attributes) {
    void* memory = XMemAllocDefault(bytes, attributes);
    if (memory) {
        std::memset(memory, 0xA5, bytes);
        ++reused_allocations;
    }
    return memory;
}
void WINAPI release_title_memory(void* memory, UINT64 attributes) { XMemFreeDefault(memory, attributes); }

// A table with slots the title never writes, in a heap whose title memory held
// leftover bytes. The root declares t0..t3 and the program reads only t0, which
// D3D12 allows. The unwritten slots must read as no descriptor; refusing the
// table skips the dispatch and leaves the sentinel.
void unwritten_descriptors(ID3D12Device* device) {
    constexpr UINT heap_slots = 16, table = 4, sentinel = 0xdeadbeefu;
    constexpr UINT key = 0x1234u, salt = 0x50005u, word = 0x2au;
    constexpr UINT expected = word * 131u + key * 17u + salt;
    D3D12_DESCRIPTOR_RANGE ranges[]{{D3D12_DESCRIPTOR_RANGE_TYPE_CBV, 1, 0, 0, 0},
                                    {D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 4, 0, 0, 1},
                                    {D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 1, 0, 0, 5}};
    D3D12_ROOT_PARAMETER param{};
    param.ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
    param.DescriptorTable = {UINT(std::size(ranges)), ranges};
    D3D12_ROOT_SIGNATURE_DESC sig{1, &param, 0, nullptr, D3D12_ROOT_SIGNATURE_FLAG_NONE};
    ComPtr<ID3DBlob> blob;
    check(D3D12SerializeRootSignature(&sig, D3D_ROOT_SIGNATURE_VERSION_1, &blob, nullptr),
          "serialize unwritten-slot root signature");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0, blob->GetBufferPointer(), blob->GetBufferSize(), IID_PPV_ARGS(&root)),
          "create unwritten-slot root signature");
    D3D12_COMPUTE_PIPELINE_STATE_DESC pso_desc{};
    pso_desc.pRootSignature = root.Get();
    pso_desc.CS = {reclaim_dxil, sizeof(reclaim_dxil)};
    ComPtr<ID3D12PipelineState> pipeline;
    check(method<CreateComputePipeline>(device, device_create_compute)(device, &pso_desc, 0, nullptr,
                                                                        IID_PPV_ARGS(&pipeline)),
          "create unwritten-slot compute pipeline");

    const UINT constants[64]{key, salt};
    auto cb = buffer(device, D3D12_HEAP_TYPE_UPLOAD, sizeof(constants), D3D12_RESOURCE_STATE_GENERIC_READ);
    auto source = buffer(device, D3D12_HEAP_TYPE_UPLOAD, sizeof(UINT), D3D12_RESOURCE_STATE_GENERIC_READ);
    auto seed = buffer(device, D3D12_HEAP_TYPE_UPLOAD, sizeof(UINT), D3D12_RESOURCE_STATE_GENERIC_READ);
    auto output = buffer(device, D3D12_HEAP_TYPE_DEFAULT, sizeof(UINT), D3D12_RESOURCE_STATE_COPY_DEST,
                         D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
    auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, sizeof(UINT), D3D12_RESOURCE_STATE_COPY_DEST);
    compute_fixture::upload(cb.Get(), constants, sizeof(constants));
    compute_fixture::upload(source.Get(), &word, sizeof(word));
    compute_fixture::upload(seed.Get(), &sentinel, sizeof(sentinel));

    D3D12_DESCRIPTOR_HEAP_DESC heap_desc{D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, heap_slots,
                                         D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    ComPtr<ID3D12DescriptorHeap> heap;
    reused_allocations = 0;
    XMemSetAllocationHooks(reused_title_memory, release_title_memory);
    const HRESULT created = device->CreateDescriptorHeap(&heap_desc, IID_PPV_ARGS(&heap));
    XMemSetAllocationHooks(nullptr, nullptr);
    check(created, "create view heap in reused title memory");
    if (!reused_allocations) throw std::runtime_error("the view heap did not take title memory");
    const UINT stride = device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
    const SIZE_T cpu_base = method<frame_abi::CpuHandle>(heap.Get(), 9)(heap.Get());
    const UINT64 gpu_base = method<GpuHandle>(heap.Get(), 10)(heap.Get());
    auto cpu_at = [&](UINT slot) -> D3D12_CPU_DESCRIPTOR_HANDLE { return {cpu_base + SIZE_T(table + slot) * stride}; };

    // Slots 0 (b0), 1 (t0) and 5 (u0). Slots 2 to 4 (t1..t3) are never written.
    D3D12_CONSTANT_BUFFER_VIEW_DESC cbv{cb->GetGPUVirtualAddress(), sizeof(constants)};
    device->CreateConstantBufferView(&cbv, cpu_at(0));
    D3D12_SHADER_RESOURCE_VIEW_DESC srv{};
    srv.ViewDimension = D3D12_SRV_DIMENSION_BUFFER;
    srv.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    srv.Buffer.NumElements = 1;
    srv.Buffer.StructureByteStride = sizeof(UINT);
    device->CreateShaderResourceView(source.Get(), &srv, cpu_at(1));
    D3D12_UNORDERED_ACCESS_VIEW_DESC uav{};
    uav.ViewDimension = D3D12_UAV_DIMENSION_BUFFER;
    uav.Buffer.NumElements = 1;
    uav.Buffer.StructureByteStride = sizeof(UINT);
    device->CreateUnorderedAccessView(output.Get(), nullptr, &uav, cpu_at(5));

    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    ID3D12GraphicsCommandList* list = direct.list.Get();
    list->CopyBufferRegion(output.Get(), 0, seed.Get(), 0, sizeof(UINT));
    compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_COPY_DEST,
                                D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    ID3D12DescriptorHeap* heaps[]{heap.Get()};
    method<BindHeaps>(list, slot_bind_heaps)(list, 1, heaps);
    method<SetRoot>(list, slot_compute_root)(list, root.Get());
    list->SetPipelineState(pipeline.Get());
    method<SetTable>(list, slot_compute_table)(list, 0, {gpu_base + UINT64(table) * stride});
    list->Dispatch(1, 1, 1);
    compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                D3D12_RESOURCE_STATE_COPY_SOURCE);
    list->CopyBufferRegion(readback.Get(), 0, output.Get(), 0, sizeof(UINT));
    direct.submit();
    direct.finish("dispatch through a table with unwritten slots");

    D3D12_RANGE read{0, sizeof(UINT)}, empty{};
    void* mapped = nullptr;
    check(readback->Map(0, &read, &mapped), "map unwritten-slot readback");
    UINT found = 0;
    std::memcpy(&found, mapped, sizeof(found));
    readback->Unmap(0, &empty);
    if (found == expected) return;
    char message[160];
    if (found == sentinel)
        std::snprintf(message, sizeof(message), "output kept its sentinel: the table was refused and the dispatch skipped");
    else
        std::snprintf(message, sizeof(message), "expected %#x, found %#x", expected, found);
    throw std::runtime_error(message);
}

// A zero-parameter direct-indexed compute root, with the requested UAV at
// logical slot 80. The paged trial caps native residency at 64 descriptors.
// Check the entire buffer, including decoys and guard words. With `table`, the
// root also has a bound table of four UAVs the program does not read.
void bindless_compute_with(ID3D12Device* device, bool table) {
    D3D12_DESCRIPTOR_RANGE1 range{D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 4, 0, 0, D3D12_DESCRIPTOR_RANGE_FLAG_NONE, 0};
    D3D12_ROOT_PARAMETER1 parameter{};
    parameter.ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
    parameter.DescriptorTable = {1, &range};
    D3D12_VERSIONED_ROOT_SIGNATURE_DESC signature{};
    signature.Version = D3D_ROOT_SIGNATURE_VERSION_1_1;
    signature.Desc_1_1 = {table ? 1u : 0u, table ? &parameter : nullptr, 0, nullptr,
                          D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED};
    ComPtr<ID3DBlob> serialized;
    check(D3D12SerializeVersionedRootSignature(&signature, &serialized, nullptr), "serialize bindless compute root");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(),
                                      IID_PPV_ARGS(&root)), "create bindless compute root");
    D3D12_COMPUTE_PIPELINE_STATE_DESC shader{};
    shader.pRootSignature = root.Get();
    shader.CS = table ? D3D12_SHADER_BYTECODE{bindless_table_compute_shader, sizeof(bindless_table_compute_shader)}
                      : D3D12_SHADER_BYTECODE{bindless_compute_shader, sizeof(bindless_compute_shader)};
    ComPtr<ID3D12PipelineState> pipeline;
    check(method<CreateComputePipeline>(device, device_create_compute)(device, &shader, 0, nullptr,
          IID_PPV_ARGS(&pipeline)), "create translated bindless compute pipeline");
    constexpr UINT views = 16, view_words = 64, decoy = 0xD0000000;
    constexpr UINT64 bytes = views * view_words * sizeof(UINT);
    std::vector<UINT> words(views * view_words);
    for (UINT i = 0; i < views; ++i) words[i * view_words] = decoy | i;
    auto seed = buffer(device, D3D12_HEAP_TYPE_UPLOAD, bytes, D3D12_RESOURCE_STATE_GENERIC_READ);
    compute_fixture::upload(seed.Get(), words.data(), size_t(bytes));
    auto output = buffer(device, D3D12_HEAP_TYPE_DEFAULT, bytes, D3D12_RESOURCE_STATE_COPY_DEST,
                         D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
    auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, bytes, D3D12_RESOURCE_STATE_COPY_DEST);
    D3D12_DESCRIPTOR_HEAP_DESC heap_desc{D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 128,
                                         D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    ComPtr<ID3D12DescriptorHeap> heap;
    check(device->CreateDescriptorHeap(&heap_desc, IID_PPV_ARGS(&heap)), "create bindless view heap");
    const UINT stride = device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
    const SIZE_T cpu_base = method<frame_abi::CpuHandle>(heap.Get(), 9)(heap.Get());
    for (UINT i = 0; i < views; ++i) {
        D3D12_UNORDERED_ACCESS_VIEW_DESC view{};
        view.ViewDimension = D3D12_UAV_DIMENSION_BUFFER;
        view.Format = DXGI_FORMAT_R32_TYPELESS;
        view.Buffer.FirstElement = UINT64(i) * view_words;
        view.Buffer.NumElements = view_words;
        view.Buffer.Flags = D3D12_BUFFER_UAV_FLAG_RAW;
        device->CreateUnorderedAccessView(output.Get(), nullptr, &view,
                                         {cpu_base + SIZE_T(bindless_index + i) * stride});
    }
    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    auto list = direct.list.Get();
    list->CopyBufferRegion(output.Get(), 0, seed.Get(), 0, bytes);
    compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_COPY_DEST,
                                D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    ID3D12DescriptorHeap* heaps[]{heap.Get()};
    method<BindHeaps>(list, slot_bind_heaps)(list, 1, heaps);
    method<SetRoot>(list, slot_compute_root)(list, root.Get());
    list->SetPipelineState(pipeline.Get());
    if (table) method<SetTable>(list, slot_compute_table)(list, 0, {method<GpuHandle>(heap.Get(), 10)(heap.Get())});
    list->Dispatch(1, 1, 1);
    compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                D3D12_RESOURCE_STATE_COPY_SOURCE);
    list->CopyBufferRegion(readback.Get(), 0, output.Get(), 0, bytes);
    direct.submit();
    direct.finish("bindless compute dispatch");
    D3D12_RANGE read{0, SIZE_T(bytes)}, empty{};
    void* mapped = nullptr;
    check(readback->Map(0, &read, &mapped), "map bindless compute readback");
    std::vector<UINT> got(words.size());
    std::memcpy(got.data(), mapped, size_t(bytes));
    readback->Unmap(0, &empty);
    words[0] = bindless_expected;
    if (got != words) throw std::runtime_error("bindless compute output or guard words differ");
}

void bindless_compute(ID3D12Device* device) { bindless_compute_with(device, false); }
void bindless_table_compute(ID3D12Device* device) { bindless_compute_with(device, true); }

// A direct-indexed compute program samples the heap texture with a linear clamp
// sampler it builds from literal words, which the zero-parameter root does not
// declare, and stores the value through a heap UAV (`shader_fixture.py`). The
// provider must add that state to the root it builds. The sample lies a quarter
// of the way between the texels, so a point filter reads another value.
void bindless_sampler(ID3D12Device* device) {
    D3D12_VERSIONED_ROOT_SIGNATURE_DESC signature{};
    signature.Version = D3D_ROOT_SIGNATURE_VERSION_1_1;
    signature.Desc_1_1 = {0, nullptr, 0, nullptr, D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED};
    ComPtr<ID3DBlob> serialized;
    check(D3D12SerializeVersionedRootSignature(&signature, &serialized, nullptr), "serialize bindless sampler root");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(),
                                      IID_PPV_ARGS(&root)), "create bindless sampler root");
    D3D12_COMPUTE_PIPELINE_STATE_DESC shader{};
    shader.pRootSignature = root.Get();
    shader.CS = {bindless_sampler_shader, sizeof(bindless_sampler_shader)};
    ComPtr<ID3D12PipelineState> pipeline;
    check(method<CreateComputePipeline>(device, device_create_compute)(device, &shader, 0, nullptr,
          IID_PPV_ARGS(&pipeline)), "create translated bindless sampler pipeline");
    auto texture = sampled_texture(device, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);

    constexpr UINT count = 64, decoy = 0xD0000000;
    constexpr UINT64 bytes = count * sizeof(UINT);
    std::vector<UINT> words(count);
    for (UINT i = 0; i < count; ++i) words[i] = decoy | i;
    auto seed = buffer(device, D3D12_HEAP_TYPE_UPLOAD, bytes, D3D12_RESOURCE_STATE_GENERIC_READ);
    compute_fixture::upload(seed.Get(), words.data(), size_t(bytes));
    auto output = buffer(device, D3D12_HEAP_TYPE_DEFAULT, bytes, D3D12_RESOURCE_STATE_COPY_DEST,
                         D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
    auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, bytes, D3D12_RESOURCE_STATE_COPY_DEST);
    D3D12_DESCRIPTOR_HEAP_DESC heap_desc{D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 128,
                                         D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    ComPtr<ID3D12DescriptorHeap> heap;
    check(device->CreateDescriptorHeap(&heap_desc, IID_PPV_ARGS(&heap)), "create bindless sampler heap");
    const UINT stride = device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
    const SIZE_T cpu_base = method<frame_abi::CpuHandle>(heap.Get(), 9)(heap.Get());
    D3D12_SHADER_RESOURCE_VIEW_DESC srv{};
    srv.Format = DXGI_FORMAT_R32_FLOAT;
    srv.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE2D;
    srv.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    srv.Texture2D.MipLevels = 1;
    device->CreateShaderResourceView(texture.Get(), &srv, {cpu_base + SIZE_T(bindless_texture) * stride});
    D3D12_UNORDERED_ACCESS_VIEW_DESC view{};
    view.ViewDimension = D3D12_UAV_DIMENSION_BUFFER;
    view.Format = DXGI_FORMAT_R32_TYPELESS;
    view.Buffer.NumElements = count;
    view.Buffer.Flags = D3D12_BUFFER_UAV_FLAG_RAW;
    device->CreateUnorderedAccessView(output.Get(), nullptr, &view, {cpu_base + SIZE_T(bindless_output) * stride});

    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    auto list = direct.list.Get();
    list->CopyBufferRegion(output.Get(), 0, seed.Get(), 0, bytes);
    compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_COPY_DEST,
                                D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    ID3D12DescriptorHeap* heaps[]{heap.Get()};
    method<BindHeaps>(list, slot_bind_heaps)(list, 1, heaps);
    method<SetRoot>(list, slot_compute_root)(list, root.Get());
    list->SetPipelineState(pipeline.Get());
    list->Dispatch(1, 1, 1);
    compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                D3D12_RESOURCE_STATE_COPY_SOURCE);
    list->CopyBufferRegion(readback.Get(), 0, output.Get(), 0, bytes);
    direct.submit();
    direct.finish("bindless sampler dispatch");
    D3D12_RANGE read{0, SIZE_T(bytes)}, empty{};
    void* mapped = nullptr;
    check(readback->Map(0, &read, &mapped), "map bindless sampler readback");
    std::vector<UINT> got(count);
    std::memcpy(got.data(), mapped, size_t(bytes));
    readback->Unmap(0, &empty);
    float found;
    std::memcpy(&found, got.data(), sizeof(found));
    if (found != raster_sampler_linear) {
        char message[200];
        if (got[0] == words[0])
            std::snprintf(message, sizeof(message), "word 0 kept its decoy: a stand-in pipeline or a skipped dispatch");
        else
            std::snprintf(message, sizeof(message), "expected %g, found %g (%#x)", raster_sampler_linear, found, got[0]);
        throw std::runtime_error(message);
    }
    if (!std::equal(got.begin() + 1, got.end(), words.begin() + 1))
        throw std::runtime_error("bindless sampler guard words differ");
}

// What the two heap-texture scenarios share: a zero-parameter direct-indexed
// root, a console compute program under it, and a guest texture whose only
// view is a UAV in the resource heap, over title memory filled with one word.
struct HeapTexture {
    static constexpr UINT side = 64, before = 0x11111111;
    static constexpr size_t words = 4 * 65536 / sizeof(UINT);
    Memory memory{4 * 65536};
    ComPtr<ID3D12RootSignature> root;
    ComPtr<ID3D12PipelineState> pipeline;
    ComPtr<ID3D12Resource> texture;
    ComPtr<ID3D12DescriptorHeap> heap;
    SIZE_T cpu_base{};
    UINT stride{};

    HeapTexture(ID3D12Device* device, const unsigned char* program, size_t size) {
        D3D12_VERSIONED_ROOT_SIGNATURE_DESC signature{};
        signature.Version = D3D_ROOT_SIGNATURE_VERSION_1_1;
        signature.Desc_1_1 = {0, nullptr, 0, nullptr, D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED};
        ComPtr<ID3DBlob> serialized;
        check(D3D12SerializeVersionedRootSignature(&signature, &serialized, nullptr), "serialize heap texture root");
        check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(),
                                          IID_PPV_ARGS(&root)), "create heap texture root");
        D3D12_COMPUTE_PIPELINE_STATE_DESC shader{};
        shader.pRootSignature = root.Get();
        shader.CS = {program, size};
        check(method<CreateComputePipeline>(device, device_create_compute)(device, &shader, 0, nullptr,
              IID_PPV_ARGS(&pipeline)), "create translated heap texture pipeline");
        fill(before);
        D3D12_RESOURCE_DESC desc{};
        desc.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
        desc.Width = side;
        desc.Height = side;
        desc.DepthOrArraySize = 1;
        desc.MipLevels = 1;
        desc.SampleDesc.Count = 1;
        desc.Format = DXGI_FORMAT_R32_UINT;
        desc.Layout = static_cast<D3D12_TEXTURE_LAYOUT>(0x105);
        desc.Flags = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
        check(method<Place>(device, device_place)(device, memory.address(), &desc, D3D12_RESOURCE_STATE_COMMON,
                                                  nullptr, IID_PPV_ARGS(&texture)), "place heap texture");
        D3D12_DESCRIPTOR_HEAP_DESC heap_desc{D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 128,
                                             D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
        check(device->CreateDescriptorHeap(&heap_desc, IID_PPV_ARGS(&heap)), "create heap texture view heap");
        stride = device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
        cpu_base = method<frame_abi::CpuHandle>(heap.Get(), 9)(heap.Get());
        D3D12_UNORDERED_ACCESS_VIEW_DESC view{};
        view.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2D;
        view.Format = DXGI_FORMAT_R32_UINT;
        device->CreateUnorderedAccessView(texture.Get(), nullptr, &view, at(bindless_store_texture));
    }
    D3D12_CPU_DESCRIPTOR_HANDLE at(UINT index) const { return {cpu_base + SIZE_T(index) * stride}; }
    UINT* guest() const { return reinterpret_cast<UINT*>(memory.base); }
    void fill(UINT word) { std::fill_n(guest(), words, word); }
    // Words of the title memory that hold `word`.
    size_t count(UINT word) const { return std::count(guest(), guest() + words, word); }
    void dispatch(ID3D12GraphicsCommandList* list) const {
        ID3D12DescriptorHeap* heaps[]{heap.Get()};
        method<BindHeaps>(list, slot_bind_heaps)(list, 1, heaps);
        method<SetRoot>(list, slot_compute_root)(list, root.Get());
        list->SetPipelineState(pipeline.Get());
        list->Dispatch(1, 1, 1);
    }
};

// A direct-indexed program can reach every view in the resource heap, so the
// provider cannot tell which textures it uses. This one stores to a buffer and
// writes no image, so it cannot have written the heap's UAV texture, and the
// provider must not write that texture's GPU contents back over title memory.
// The dispatch waits for a label while the title rewrites the texture's pages:
// a write-back after the dispatch would replace the title's new words.
void bindless_heap_read(ID3D12Device* device) {
    constexpr UINT view_words = 64, after = 0x22222222;
    constexpr UINT64 bytes = view_words * sizeof(UINT);
    HeapTexture scene(device, bindless_compute_shader, sizeof(bindless_compute_shader));
    Memory label(65536);
    auto output = buffer(device, D3D12_HEAP_TYPE_DEFAULT, bytes, D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                         D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
    auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, bytes, D3D12_RESOURCE_STATE_COPY_DEST);
    D3D12_UNORDERED_ACCESS_VIEW_DESC view{};
    view.ViewDimension = D3D12_UAV_DIMENSION_BUFFER;
    view.Format = DXGI_FORMAT_R32_TYPELESS;
    view.Buffer.NumElements = view_words;
    view.Buffer.Flags = D3D12_BUFFER_UAV_FLAG_RAW;
    device->CreateUnorderedAccessView(output.Get(), nullptr, &view, scene.at(bindless_index));

    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    Queue compute(device, D3D12_COMMAND_LIST_TYPE_COMPUTE);
    auto list = direct.list.Get();
    direct.wait64(label.address(), 1);
    scene.dispatch(list);
    compute_fixture::transition(list, output.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                D3D12_RESOURCE_STATE_COPY_SOURCE);
    list->CopyBufferRegion(readback.Get(), 0, output.Get(), 0, bytes);
    direct.submit();
    // The dispatch is held. The title now owns these pages again.
    scene.fill(after);
    compute.write64(label.address(), 1);
    compute.submit();
    compute.finish("compute queue releasing the dispatch");
    direct.finish("direct-indexed dispatch beside a heap texture");

    D3D12_RANGE read{0, SIZE_T(bytes)}, empty{};
    void* mapped = nullptr;
    check(readback->Map(0, &read, &mapped), "map heap read output");
    UINT stored;
    std::memcpy(&stored, mapped, sizeof(stored));
    readback->Unmap(0, &empty);
    if (stored != bindless_expected)
        throw std::runtime_error("the dispatch did not store its word: a stand-in pipeline or a skipped dispatch");
    if (const size_t kept = scene.count(after); kept != HeapTexture::words) {
        char message[200];
        std::snprintf(message, sizeof(message),
                      "the provider wrote back a texture the program cannot write: %zu of %zu title words replaced",
                      HeapTexture::words - kept, HeapTexture::words);
        throw std::runtime_error(message);
    }
}

// The same heap texture under a program that does store to it, one word to one
// texel. The provider must still publish that write to title memory, and the
// GPU image must hold it.
void bindless_texture_store(ID3D12Device* device) {
    constexpr UINT side = HeapTexture::side, pitch = side * sizeof(UINT);
    constexpr UINT64 bytes = UINT64(pitch) * side;
    HeapTexture scene(device, bindless_texture_store_shader, sizeof(bindless_texture_store_shader));
    auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, bytes, D3D12_RESOURCE_STATE_COPY_DEST);
    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    scene.dispatch(direct.list.Get());
    direct.submit();
    direct.finish("direct-indexed texture store");
    const size_t published = scene.count(bindless_store_expected);
    const size_t untouched = scene.count(HeapTexture::before);

    D3D12_TEXTURE_COPY_LOCATION image{}, linear{};
    image.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
    image.pResource = scene.texture.Get();
    linear.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
    linear.pResource = readback.Get();
    linear.PlacedFootprint.Footprint = {DXGI_FORMAT_R32_UINT, side, side, 1, pitch};
    direct.reopen();
    method<frame_abi::Copy>(direct.list.Get(), slot_copy_texture)(
        direct.list.Get(), &linear, 0, 0, 0, &image, nullptr, 0);
    direct.submit();
    direct.finish("reading the stored texture");
    D3D12_RANGE read{0, SIZE_T(bytes)}, empty{};
    void* mapped = nullptr;
    check(readback->Map(0, &read, &mapped), "map stored texture");
    std::vector<UINT> texels(size_t(side) * side);
    std::memcpy(texels.data(), mapped, size_t(bytes));
    readback->Unmap(0, &empty);
    char message[200];
    for (UINT y = 0; y < side; ++y)
        for (UINT x = 0; x < side; ++x) {
            const UINT expected = x == bindless_store_x && y == bindless_store_y ? bindless_store_expected
                                                                                 : HeapTexture::before;
            if (const UINT found = texels[size_t(y) * side + x]; found != expected) {
                std::snprintf(message, sizeof(message), "texel (%u, %u): expected %#x, found %#x", x, y, expected, found);
                throw std::runtime_error(message);
            }
        }
    if (published != 1 || untouched != HeapTexture::words - 1) {
        std::snprintf(message, sizeof(message),
                      "title memory holds the stored word %zu times and its old word %zu times of %zu: "
                      "the store was not published exactly",
                      published, untouched, HeapTexture::words);
        throw std::runtime_error(message);
    }
}

// A guest texture of 64x64 words over title memory filled with one word.
struct GuestWords {
    static constexpr UINT side = 64;
    static constexpr size_t words = 4 * 65536 / sizeof(UINT);
    Memory memory{4 * 65536};
    ComPtr<ID3D12Resource> texture;
    GuestWords(ID3D12Device* device, DXGI_FORMAT format, UINT word) {
        fill(word);
        D3D12_RESOURCE_DESC desc{};
        desc.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
        desc.Width = side;
        desc.Height = side;
        desc.DepthOrArraySize = 1;
        desc.MipLevels = 1;
        desc.SampleDesc.Count = 1;
        desc.Format = format;
        desc.Layout = static_cast<D3D12_TEXTURE_LAYOUT>(0x105);
        desc.Flags = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
        check(method<Place>(device, device_place)(device, memory.address(), &desc, D3D12_RESOURCE_STATE_COMMON,
                                                  nullptr, IID_PPV_ARGS(&texture)), "place guest texture of words");
    }
    UINT* guest() const { return reinterpret_cast<UINT*>(memory.base); }
    void fill(UINT word) { std::fill_n(guest(), words, word); }
    size_t count(UINT word) const { return std::count(guest(), guest() + words, word); }
};

// A program under a root that allows direct heap indexing stores one word to
// the texture at slot 1 of its descriptor table, and uses nothing else. The
// provider must publish that texture to title memory, and must leave alone a
// second texture whose UAV sits elsewhere in the heap: the dispatch waits for
// a label while the title rewrites the second texture's pages, and a
// write-back after the dispatch would replace the title's new words.
void bindless_table_store(ID3D12Device* device) {
    constexpr UINT side = GuestWords::side, pitch = side * sizeof(UINT), before = 0x11111111, after = 0x22222222;
    constexpr UINT table_start = 70, elsewhere = 100;
    constexpr UINT64 bytes = UINT64(pitch) * side;
    // The root names the table's four views u0-u3, as the program's records
    // do, so the provider keeps the title's root and serves it through the
    // resource index map. The provider reflects version 1.1 roots only.
    D3D12_DESCRIPTOR_RANGE1 range{D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 4, 0, 0,
                                  D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE, 0};
    D3D12_ROOT_PARAMETER1 table{};
    table.ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
    table.DescriptorTable = {1, &range};
    D3D12_VERSIONED_ROOT_SIGNATURE_DESC signature{};
    signature.Version = D3D_ROOT_SIGNATURE_VERSION_1_1;
    signature.Desc_1_1 = {1, &table, 0, nullptr, D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED};
    ComPtr<ID3DBlob> serialized;
    check(D3D12SerializeVersionedRootSignature(&signature, &serialized, nullptr),
          "serialize direct-indexed table root");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(),
                                      IID_PPV_ARGS(&root)), "create direct-indexed table root");
    D3D12_COMPUTE_PIPELINE_STATE_DESC shader{};
    shader.pRootSignature = root.Get();
    shader.CS = {bindless_table_store_shader, sizeof(bindless_table_store_shader)};
    ComPtr<ID3D12PipelineState> pipeline;
    check(method<CreateComputePipeline>(device, device_create_compute)(device, &shader, 0, nullptr,
          IID_PPV_ARGS(&pipeline)), "create translated table store pipeline");

    GuestWords stored(device, DXGI_FORMAT_R32_FLOAT, before), other(device, DXGI_FORMAT_R32_FLOAT, before);
    Memory label(65536);
    // A title's allocator hands out memory it has not cleared. Leave some of
    // the size of this heap's descriptors behind, so that the table's unwritten
    // slots start with leftover bytes, as they can in a title.
    {
        constexpr size_t heap_bytes = 128 * 32;
        void* blocks[8];
        for (auto& block : blocks) {
            block = HeapAlloc(GetProcessHeap(), 0, heap_bytes);
            if (block) std::memset(block, 0xA5, heap_bytes);
        }
        for (void* block : blocks)
            if (block) HeapFree(GetProcessHeap(), 0, block);
    }
    D3D12_DESCRIPTOR_HEAP_DESC heap_desc{D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 128,
                                         D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    ComPtr<ID3D12DescriptorHeap> heap;
    check(device->CreateDescriptorHeap(&heap_desc, IID_PPV_ARGS(&heap)), "create table store view heap");
    const UINT stride = device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
    const SIZE_T cpu_base = method<frame_abi::CpuHandle>(heap.Get(), 9)(heap.Get());
    const UINT64 gpu_base = method<GpuHandle>(heap.Get(), 10)(heap.Get());
    D3D12_UNORDERED_ACCESS_VIEW_DESC view{};
    view.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2D;
    view.Format = DXGI_FORMAT_R32_FLOAT;
    device->CreateUnorderedAccessView(stored.texture.Get(), nullptr, &view,
                                     {cpu_base + SIZE_T(table_start + bindless_table_slot) * stride});
    device->CreateUnorderedAccessView(other.texture.Get(), nullptr, &view, {cpu_base + SIZE_T(elsewhere) * stride});

    Queue direct(device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    Queue compute(device, D3D12_COMMAND_LIST_TYPE_COMPUTE);
    auto list = direct.list.Get();
    direct.wait64(label.address(), 1);
    ID3D12DescriptorHeap* heaps[]{heap.Get()};
    method<BindHeaps>(list, slot_bind_heaps)(list, 1, heaps);
    method<SetRoot>(list, slot_compute_root)(list, root.Get());
    method<SetTable>(list, slot_compute_table)(list, 0, {gpu_base + UINT64(table_start) * stride});
    list->SetPipelineState(pipeline.Get());
    list->Dispatch(1, 1, 1);
    direct.submit();
    // The dispatch is held. The title now owns the second texture's pages again.
    other.fill(after);
    compute.write64(label.address(), 1);
    compute.submit();
    compute.finish("compute queue releasing the table store");
    direct.finish("table store under a direct-indexed root");
    const size_t published = stored.count(bindless_table_expected), untouched = stored.count(before);
    const size_t kept = other.count(after);

    auto readback = buffer(device, D3D12_HEAP_TYPE_READBACK, bytes, D3D12_RESOURCE_STATE_COPY_DEST);
    D3D12_TEXTURE_COPY_LOCATION image{}, linear{};
    image.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
    image.pResource = stored.texture.Get();
    linear.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
    linear.pResource = readback.Get();
    linear.PlacedFootprint.Footprint = {DXGI_FORMAT_R32_FLOAT, side, side, 1, pitch};
    direct.reopen();
    method<frame_abi::Copy>(direct.list.Get(), slot_copy_texture)(
        direct.list.Get(), &linear, 0, 0, 0, &image, nullptr, 0);
    direct.submit();
    direct.finish("reading the table-stored texture");
    D3D12_RANGE read{0, SIZE_T(bytes)}, empty{};
    void* mapped = nullptr;
    check(readback->Map(0, &read, &mapped), "map table-stored texture");
    std::vector<UINT> texels(size_t(side) * side);
    std::memcpy(texels.data(), mapped, size_t(bytes));
    readback->Unmap(0, &empty);
    char message[200];
    for (UINT y = 0; y < side; ++y)
        for (UINT x = 0; x < side; ++x) {
            const UINT expected = x == bindless_store_x && y == bindless_store_y ? bindless_table_expected : before;
            if (const UINT found = texels[size_t(y) * side + x]; found != expected) {
                std::snprintf(message, sizeof(message), "texel (%u, %u): expected %#x, found %#x%s", x, y, expected,
                              found, expected != before && found == before ? " (a stand-in or a skipped dispatch)" : "");
                throw std::runtime_error(message);
            }
        }
    if (published != 1 || untouched != GuestWords::words - 1) {
        std::snprintf(message, sizeof(message),
                      "title memory holds the stored word %zu times and its old word %zu times of %zu: "
                      "the table store was not published exactly", published, untouched, GuestWords::words);
        throw std::runtime_error(message);
    }
    if (kept != GuestWords::words) {
        std::snprintf(message, sizeof(message),
                      "the provider wrote back a texture the program cannot reach: %zu of %zu title words replaced",
                      GuestWords::words - kept, GuestWords::words);
        throw std::runtime_error(message);
    }
}

struct Scenario {
    const wchar_t* name;
    void (*run)(ID3D12Device*);
};
constexpr Scenario scenarios[] = {
    {L"labels", labels},
    {L"cross-queue", cross_queue},
    {L"compute-frame", compute_frame},
    {L"skipped-command", skipped_command},
    {L"copy-memory", copy_memory},
    {L"direct-copy-wait", direct_copy_wait},
    {L"shared-texture", shared_texture},
    {L"indirect-dispatch", indirect_dispatch},
    {L"gpu-indirect-dispatch", gpu_indirect_dispatch},
    {L"gpu-indirect-order", gpu_indirect_order},
    {L"mesh-signature", mesh_signature},
    {L"mesh-signature-none", mesh_signature_none},
    {L"dispatch-wait", dispatch_wait},
    {L"texture-readback", texture_readback},
    {L"texture-publication", texture_publication},
    {L"ray-tracing-builder", ray_tracing_builder},
    {L"ray-tracing-guest-memory", ray_tracing_guest_memory},
    {L"ray-tracing-rehydrate", ray_tracing_rehydrate},
    {L"thread-dimensions", thread_dimensions},
    {L"descriptor-reclaim", descriptor_reclaim},
    {L"unwritten-descriptors", unwritten_descriptors},
    {L"xbox-shader", xbox_shader_scenario},
    {L"wave64-shader", wave64_shader_scenario},
    {L"mixed-lane", mixed_lane_scenario},
    {L"raster-tables", raster_tables},
    {L"lane-spill-interp", lane_spill_interp},
    {L"wave-alloc", wave_alloc_scenario},
    {L"raster-samplers", raster_samplers},
    {L"compute-root", compute_root},
    {L"mesh-samplers", mesh_samplers},
    {L"mesh-sampler-single", mesh_sampler_single},
    {L"mesh-sampler-rootless", mesh_sampler_rootless},
    {L"compute-root-recovery", compute_root_recovery},
    {L"compute-root-srv", compute_root_srv},
    {L"bindless-compute", bindless_compute},
    {L"bindless-sampler", bindless_sampler},
    {L"bindless-heap-read", bindless_heap_read},
    {L"bindless-texture-store", bindless_texture_store},
    {L"bindless-table-store", bindless_table_store},
    {L"bindless-table-compute", bindless_table_compute},
    {L"raster-heap-stride", raster_heap_stride},
    {L"large-dispatch", large_dispatch},
    {L"late-pipeline", late_pipeline},
    {L"compute-heap-stride", compute_heap_stride},
    {L"raster-sampler-waterfall", raster_sampler_waterfall},
};

// A fresh Scarlett device on the requested adapter. The factory is reached
// through the Xbox device, so the game imports no desktop graphics library.
ComPtr<ID3D12Device> create_scarlett_device(const wchar_t* requested, char (&pci)[16]) {
    auto create = [](IUnknown* adapter, ComPtr<ID3D12Device>& out) {
        return D3D12XboxCreateDevice(adapter, scarlett_profile.data(), __uuidof(ID3D12Device11),
                                     reinterpret_cast<void**>(out.ReleaseAndGetAddressOf()));
    };
    auto matches = [&](IDXGIAdapter1* adapter) {
        DXGI_ADAPTER_DESC1 desc{};
        check(adapter->GetDesc1(&desc), "adapter description");
        std::snprintf(pci, sizeof(pci), "%04x:%04x", desc.VendorId, desc.DeviceId);
        wchar_t found[16];
        std::swprintf(found, 16, L"%04x:%04x", desc.VendorId, desc.DeviceId);
        return !(desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE) && (!requested || !_wcsicmp(requested, found));
    };
    ComPtr<ID3D12Device> device;
    check(create(nullptr, device), "Scarlett device");
    ComPtr<IDXGIDevice1> dxgi;
    check(device.As(&dxgi), "Xbox device DXGI interface");
    ComPtr<IDXGIAdapter> adapter;
    check(dxgi->GetAdapter(&adapter), "Xbox device adapter");
    ComPtr<IDXGIAdapter1> adapter1;
    check(adapter.As(&adapter1), "Xbox device adapter interface");
    if (matches(adapter1.Get())) return device;
    ComPtr<IDXGIFactory1> factory;
    check(adapter->GetParent(IID_PPV_ARGS(&factory)), "Xbox adapter parent");
    for (UINT n = 0;; ++n) {
        ComPtr<IDXGIAdapter1> candidate;
        const HRESULT hr = factory->EnumAdapters1(n, &candidate);
        if (hr == DXGI_ERROR_NOT_FOUND) throw std::runtime_error("no matching Xbox D3D12 hardware adapter");
        check(hr, "adapter enumeration");
        if (!matches(candidate.Get())) continue;
        check(create(candidate.Get(), device), "Scarlett device on selected adapter");
        return device;
    }
}

std::string narrow(const wchar_t* text) {
    std::string result;
    while (*text) result += static_cast<char>(*text++);
    return result;
}
}

const wchar_t* const selftest_scenarios[] = {L"labels",            L"cross-queue",      L"compute-frame",
                                             L"skipped-command",   L"copy-memory",      L"direct-copy-wait", L"shared-texture",
                                             L"indirect-dispatch", L"gpu-indirect-dispatch", L"gpu-indirect-order",
                                             L"mesh-signature", L"mesh-signature-none",
                                             L"dispatch-wait", L"texture-readback", L"texture-publication",
                                             L"ray-tracing-builder", L"ray-tracing-guest-memory", L"ray-tracing-rehydrate",
                                             L"thread-dimensions", L"descriptor-reclaim", L"unwritten-descriptors",
                                             L"xbox-shader", L"wave64-shader", L"mixed-lane",
                                             L"raster-tables", L"lane-spill-interp", L"wave-alloc", L"raster-samplers", L"compute-root",
                                             L"mesh-samplers", L"mesh-sampler-single", L"mesh-sampler-rootless",
                                             L"compute-root-recovery", L"compute-root-srv", L"bindless-compute",
                                             L"bindless-sampler", L"bindless-heap-read", L"bindless-texture-store",
                                             L"bindless-table-store", L"bindless-table-compute",
                                             L"raster-heap-stride", L"large-dispatch",
                                             L"compute-heap-stride", L"late-pipeline",
                                             L"raster-sampler-waterfall"};
const unsigned selftest_scenario_count = static_cast<unsigned>(std::size(selftest_scenarios));
static_assert(std::size(selftest_scenarios) == std::size(scenarios));

std::string run_selftest(const wchar_t* requested, const wchar_t* adapter) {
    const bool all = !std::wcscmp(requested, L"all");
    unsigned passed = 0;
    char pci[16]{};
    for (const auto& scenario : scenarios) {
        if (!all && std::wcscmp(requested, scenario.name)) continue;
        // This one writes nothing until the launcher's compiler has translated it,
        // and mesh-samplers needs its root-aware runtime shader worker.
        // mixed-lane, raster-heap-stride, large-dispatch, compute-heap-stride, late-pipeline and raster-sampler-waterfall
        // also need the live compiler.
        // mesh-signature-none needs a host profile without mesh shaders.
        if (all && (!std::wcscmp(scenario.name, L"mesh-signature-none") || !std::wcscmp(scenario.name, L"wave64-shader") || !std::wcscmp(scenario.name, L"mixed-lane") || !std::wcscmp(scenario.name, L"raster-heap-stride") || !std::wcscmp(scenario.name, L"large-dispatch") || !std::wcscmp(scenario.name, L"compute-heap-stride") || !std::wcscmp(scenario.name, L"late-pipeline") || !std::wcscmp(scenario.name, L"raster-sampler-waterfall") || !std::wcsncmp(scenario.name, L"mesh-sampler", 12) || !std::wcsncmp(scenario.name, L"bindless-", 9)))
            continue;
        const auto name = narrow(scenario.name);
        try {
            auto device = create_scarlett_device(adapter, pci);
            scenario.run(device.Get());
        } catch (const std::exception& error) {
            throw std::runtime_error(name + ": " + error.what());
        }
        ++passed;
        // Progress, so a crash or a stuck later scenario can be located.
        std::fprintf(stderr, "lanes-selftest %s ok\n", name.c_str());
    }
    if (!passed) throw std::runtime_error("unknown scenario " + narrow(requested));
    return "lanes-selftest PASS pci=" + std::string(pci) + " scenario=" + narrow(requested) +
           " passed=" + std::to_string(passed);
}

std::string run_benchmark(const wchar_t* adapter) {
    char pci[16]{};
    Readings readings{};
    {
        auto device = create_scarlett_device(adapter, pci);
        readings = measure(device.Get());
    }
    // The game's own frame: paint the board, upload it through the Xbox command
    // list and read it back, without a window.
    std::string selected;
    auto device = create_lanes_device(adapter, selected);
    auto game = std::make_unique<Game>();
    generate_course(device.Get(), game->rows, 0x5194c0d1u);
    Screen screen(device.Get());
    const auto frames = timed_us(300, [&] {
        game->move(0);
        if (game->finish != Finish::playing) game->restart();
        screen.verify(*game);
    });
    char line[320];
    int used = std::snprintf(line, sizeof(line),
                             "lanes-benchmark PASS pci=%s submit_us=%.1f roundtrip_us=%.1f copy64k_us=%.1f "
                             "frame_us=%.1f frame_p99_us=%.1f",
                             pci, readings.submit_us, readings.roundtrip_us, readings.copy_us,
                             frames[frames.size() / 2], frames[frames.size() * 99 / 100]);
    if (readings.shader_us > 0)
        std::snprintf(line + used, sizeof(line) - used, " shader_pipeline_us=%.1f dxil_pipeline_us=%.1f",
                      readings.shader_us, readings.dxil_us);
    return line;
}
