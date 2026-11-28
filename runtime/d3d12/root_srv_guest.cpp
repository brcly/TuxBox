// Bind a root SRV inside title memory and read it far past its first page.
// A root descriptor has no size: a console shader reads title memory as far
// past it as the title placed data. ProjectScarlett reads per-instance records
// 80 KiB past one; a host that maps only the first page faults there.
#include "compute_fixture.h"
#include "frame_cli.h"
#include "root_srv.h"
namespace {
using namespace frame_abi;
using namespace compute_fixture;
using Pipeline = HRESULT(WINAPI*)(ID3D12Device*,const D3D12_COMPUTE_PIPELINE_STATE_DESC*,UINT,const void*,REFIID,void**);
using Root = void(WINAPI*)(ID3D12GraphicsCommandList*,ID3D12RootSignature*);
using View = void(WINAPI*)(ID3D12GraphicsCommandList*,UINT,UINT64);
using Signal = HRESULT(WINAPI*)(ID3D12CommandQueue*,ID3D12Fence*,UINT64);
// The bound address sits inside a 64 KiB page, with 2 MiB of title memory after it.
constexpr UINT start=0x4480;
constexpr UINT reach=2u<<20;
std::string run(const wchar_t* requested,bool) {
    DXGI_ADAPTER_DESC1 selected{};
    constexpr std::array<UINT,18> release_profile{0x20004,0,UINT_MAX,0,0,0,UINT_MAX,UINT_MAX,0,1,0,0,0,0,0,0,0,0};
    auto device=create_device(requested,selected,release_profile.data(),__uuidof(ID3D12Device11));
    D3D12_ROOT_PARAMETER params[2]{};
    params[0].ParameterType=D3D12_ROOT_PARAMETER_TYPE_SRV;
    params[1].ParameterType=D3D12_ROOT_PARAMETER_TYPE_UAV;
    D3D12_ROOT_SIGNATURE_DESC rd{2,params,0,nullptr,D3D12_ROOT_SIGNATURE_FLAG_NONE};
    ComPtr<ID3DBlob> blob;
    check(D3D12SerializeRootSignature(&rd,D3D_ROOT_SIGNATURE_VERSION_1,&blob,nullptr),"serialize root");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0,blob->GetBufferPointer(),blob->GetBufferSize(),IID_PPV_ARGS(&root)),"root");
    D3D12_COMPUTE_PIPELINE_STATE_DESC ps{};
    ps.pRootSignature=root.Get();ps.CS={root_srv_dxil,sizeof(root_srv_dxil)};
    ComPtr<ID3D12PipelineState> pipeline;
    check(method<Pipeline>(device.Get(),62)(device.Get(),&ps,0,nullptr,IID_PPV_ARGS(&pipeline)),"pipeline");
    struct Memory {
        UINT* data=static_cast<UINT*>(VirtualAlloc(nullptr,start+reach,MEM_RESERVE|MEM_COMMIT,PAGE_READWRITE));
        ~Memory(){if(data)VirtualFree(data,0,MEM_RELEASE);}
    } memory;
    if(!memory.data)throw std::runtime_error("title memory");
    for(UINT i=0;i<(start+reach)/4;++i)memory.data[i]=0x5a000000u+i*7u;
    D3D12_COMMAND_QUEUE_DESC q{};q.Type=D3D12_COMMAND_LIST_TYPE_COMPUTE;
    ComPtr<ID3D12CommandQueue> queue;check(device->CreateCommandQueue(&q,IID_PPV_ARGS(&queue)),"queue");
    ComPtr<ID3D12Fence> fence;check(device->CreateFence(0,D3D12_FENCE_FLAG_NONE,IID_PPV_ARGS(&fence)),"fence");
    constexpr UINT words=20;
    // Seed the result, so a dispatch that never writes cannot pass for one that read zeros.
    std::array<UINT,words> initial;initial.fill(0xdeadbeef);
    auto seed=buffer(device.Get(),D3D12_HEAP_TYPE_UPLOAD,sizeof(initial),D3D12_RESOURCE_STATE_GENERIC_READ);
    upload(seed.Get(),initial.data(),sizeof(initial));
    auto result=buffer(device.Get(),D3D12_HEAP_TYPE_DEFAULT,sizeof(initial),D3D12_RESOURCE_STATE_COPY_DEST,D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
    auto readback=buffer(device.Get(),D3D12_HEAP_TYPE_READBACK,sizeof(initial),D3D12_RESOURCE_STATE_COPY_DEST);
    ComPtr<ID3D12CommandAllocator> allocator;
    check(device->CreateCommandAllocator(q.Type,IID_PPV_ARGS(&allocator)),"allocator");
    const UINT desc[]{D3D12_COMMAND_LIST_TYPE_COMPUTE,0,1,0};
    ComPtr<ID3D12GraphicsCommandList> list;
    check(method<CreateList>(device.Get(),73)(device.Get(),desc,allocator.Get(),pipeline.Get(),IID_PPV_ARGS(&list)),"list");
    list->CopyBufferRegion(result.Get(),0,seed.Get(),0,sizeof(initial));
    transition(list.Get(),result.Get(),D3D12_RESOURCE_STATE_COPY_DEST,D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    // Scarlett's root binding slots are shifted by four: compute SRV 43, UAV 45.
    method<Root>(list.Get(),33)(list.Get(),root.Get());
    method<View>(list.Get(),43)(list.Get(),0,reinterpret_cast<UINT64>(memory.data)+start);
    method<View>(list.Get(),45)(list.Get(),1,result->GetGPUVirtualAddress());
    list->Dispatch(1,1,1);
    transition(list.Get(),result.Get(),D3D12_RESOURCE_STATE_UNORDERED_ACCESS,D3D12_RESOURCE_STATE_COPY_SOURCE);
    list->CopyBufferRegion(readback.Get(),0,result.Get(),0,words*4);
    check(list->Close(),"close");
    ID3D12CommandList* batch[]{list.Get()};queue->ExecuteCommandLists(1,batch);
    check(method<Signal>(queue.Get(),16)(queue.Get(),fence.Get(),1),"signal");
    wait_for_fence(fence.Get(),1);
    constexpr UINT offsets[5]{0u,0xff00u,0x10000u,0x14000u,0x100000u};
    D3D12_RANGE range{0,words*4},empty{};void* ptr{};
    check(readback->Map(0,&range,&ptr),"readback");
    const UINT* got=static_cast<const UINT*>(ptr);
    UINT right=0;std::string wrong;
    for(UINT i=0;i<5;++i){
        bool bad=false;
        for(UINT k=0;k<4;++k){
            if(got[4*i+k]==memory.data[(start+offsets[i])/4+k])++right;
            else bad=true;
        }
        if(bad)wrong+=" "+std::to_string(offsets[i]);
    }
    // The first word at each offset, read and expected (deadbeef: never written).
    char first[160];
    std::snprintf(first,sizeof(first)," (read %08x %08x %08x %08x %08x, expected %08x %08x %08x %08x %08x)",
        got[0],got[4],got[8],got[12],got[16],
        memory.data[(start+offsets[0])/4],memory.data[(start+offsets[1])/4],memory.data[(start+offsets[2])/4],
        memory.data[(start+offsets[3])/4],memory.data[(start+offsets[4])/4]);
    readback->Unmap(0,&empty);
    if(right!=words)throw std::runtime_error("root SRV words="+std::to_string(right)+"/20 wrong at offsets"+wrong+first);
    char receipt[160];
    std::snprintf(receipt,sizeof(receipt),"d3d12-root-srv-reach PASS pci=%04x:%04x words=20/20 offsets=0,65280,65536,81920,1048576",
        selected.VendorId,selected.DeviceId);
    return receipt;
}
}
int wmain(int argc,wchar_t** argv){
    return tuxbox_test::frame_main(argc,argv,"d3d12-root-srv-reach",false,run);
}
