// Independent console-ABI guest. The optional retail test takes a source,
// 64-bit offset, compressed size and independently inflated reference file.
#define WIN32_LEAN_AND_MEAN
#include <algorithm>
#include <array>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <iterator>
#include <stdexcept>
#include <unknwn.h>
#include <vector>
#include <windows.h>
#include <wrl/client.h>

using Microsoft::WRL::ComPtr;
struct QueueDesc {
  UINT64 source;
  UINT16 capacity;
  INT8 priority;
  const char *name;
};
struct Request {
  UINT64 options;
  void *dest;
  UINT64 pageOffset;
  UINT32 capacity;
  IUnknown *file;
  UINT64 offset;
  UINT32 size;
  UINT32 intermediate;
  UINT64 tag;
  const char *name;
};
struct QueueInfo {
  QueueDesc desc;
  UINT16 empty, untilSubmit;
};
struct __declspec(uuid("43704f9f-27c6-4a9f-8815-d16527894df4")) XFile
    : IUnknown {
  virtual void STDMETHODCALLTYPE Close() = 0;
  virtual HANDLE STDMETHODCALLTYPE GetHandle() = 0;
};
struct __declspec(uuid("cd66d752-2786-44f7-9070-e2a6fdad3f1b")) Status
    : IUnknown {
  virtual bool STDMETHODCALLTYPE IsComplete(UINT32) = 0;
  virtual HRESULT STDMETHODCALLTYPE GetHResult(UINT32) = 0;
};
struct __declspec(uuid("a3f6649a-8c2c-4b3a-8b6e-10186a04b9bc")) Queue
    : IUnknown {
  virtual void STDMETHODCALLTYPE EnqueueRequest(const Request *) = 0;
  virtual void STDMETHODCALLTYPE EnqueueStatus(Status *, UINT32) = 0;
  virtual void STDMETHODCALLTYPE EnqueueSignal(IUnknown *, UINT64) = 0;
  virtual void STDMETHODCALLTYPE Submit() = 0;
  virtual void STDMETHODCALLTYPE CancelRequestsWithTag(UINT64, UINT64) = 0;
  virtual void STDMETHODCALLTYPE Close() = 0;
  virtual HANDLE STDMETHODCALLTYPE GetErrorEvent() = 0;
  virtual void STDMETHODCALLTYPE RetrieveErrorRecord(void *) = 0;
  virtual void STDMETHODCALLTYPE Query(QueueInfo *) = 0;
};
struct __declspec(uuid("fb3b54ac-bf82-463a-b777-73195837864d")) Factory
    : IUnknown {
  virtual HRESULT STDMETHODCALLTYPE CreateQueue(const QueueDesc *, REFIID,
                                                void **) = 0;
  virtual HRESULT STDMETHODCALLTYPE OpenFile(const wchar_t *, REFIID,
                                             void **) = 0;
  virtual HRESULT STDMETHODCALLTYPE CreateStatusArray(UINT32, const char *,
                                                      REFIID, void **) = 0;
};
struct PriorityStats {
  UINT32 queues, peak, total, statuses, fences, pending, requests;
  UINT64 source, destination;
};
struct StorageStats {
  PriorityStats priorities[4];
  BYTE rest[64];
};
struct __declspec(uuid("62959d11-d45f-4884-a0a3-f7862ea243ae")) Factory2
    : Factory {
  virtual HRESULT STDMETHODCALLTYPE SetCpuAffinity(UINT64) = 0;
  virtual void STDMETHODCALLTYPE SetDebugFlags(UINT32) = 0;
  virtual HRESULT STDMETHODCALLTYPE SetStagingBufferSize(UINT32) = 0;
  virtual void STDMETHODCALLTYPE GetStats(StorageStats *) = 0;
};
extern "C" __declspec(dllimport) HRESULT WINAPI DStorageGetFactory(REFIID,
                                                                   void **);
static_assert(offsetof(Request, capacity) == 24 &&
              offsetof(Request, offset) == 40 && offsetof(Request, tag) == 56);

static void require(bool ok, const char *message) {
  if (!ok)
    throw std::runtime_error(message);
}
static void check(HRESULT hr, const char *message) {
  if (FAILED(hr)) {
    std::fprintf(stderr, "%s: 0x%08x\n", message, unsigned(hr));
    throw std::runtime_error(message);
  }
}
static HRESULT wait_status(Status *s, UINT i) {
  ULONGLONG deadline = GetTickCount64() + 10000;
  while (!s->IsComplete(i) && GetTickCount64() < deadline)
    Sleep(1);
  return s->GetHResult(i);
}

static void run(int argc, wchar_t **argv) {
  require(argc == 1 || argc == 2 || argc == 5 || argc == 6,
          "expected [receipt] or source offset size oracle [receipt]");
  ComPtr<Factory> f, other;
  check(DStorageGetFactory(IID_PPV_ARGS(&f)), "Xbox factory");
  check(DStorageGetFactory(IID_PPV_ARGS(&other)), "repeat factory");
  require(f.Get() == other.Get(), "factory identity");
  void *none = f.Get();
  require(DStorageGetFactory(__uuidof(Queue), &none) == E_NOINTERFACE && !none,
          "foreign factory IID");
  ComPtr<Queue> q;
  QueueDesc d{0, 0x80, 0, "Storage fixture"};
  check(f->CreateQueue(&d, IID_PPV_ARGS(&q)), "queue");
  QueueInfo info{};
  q->Query(&info);
  require(info.empty == 127 && info.untilSubmit == 64 &&
              !std::strcmp(info.desc.name, d.name),
          "queue capacity");
  ComPtr<Status> status;
  check(f->CreateStatusArray(3, "Completion", IID_PPV_ARGS(&status)), "status");
  const unsigned char compressed[] = {
      0x78, 0x9c, 0x63, 0x60, 0x64, 0x62, 0x66, 0x61, 0x65, 0x63, 0xe7, 0xe0,
      0xe4, 0xe2, 0xe6, 0xe1, 0xe5, 0xe3, 0x07, 0x00, 0x02, 0xb8, 0x00, 0x79};
  std::vector<unsigned char> expected(16);
  for (unsigned i = 0; i < expected.size(); ++i)
    expected[i] = static_cast<unsigned char>(i);
  unsigned size = sizeof(compressed);
  const wchar_t *file = L"xcrdapi-fixture.bin";
  UINT64 offset = 16;
  if (argc >= 5) {
    file = argv[1];
    offset = std::wcstoull(argv[2], nullptr, 0);
    size = std::wcstoul(argv[3], nullptr, 0);
    std::ifstream oracle(argv[4], std::ios::binary);
    require(oracle.good(), "open oracle");
    expected.assign(std::istreambuf_iterator<char>(oracle), {});
    require(expected.size() > 16, "nonempty oracle");
  } else {
    std::ofstream out(file, std::ios::binary | std::ios::trunc);
    out.write(reinterpret_cast<const char *>(expected.data()), expected.size());
    out.write(reinterpret_cast<const char *>(compressed), sizeof(compressed));
    require(out.good(), "write fixture");
  }
  ComPtr<XFile> source;
  check(f->OpenFile(file, IID_PPV_ARGS(&source)), "file");
  BY_HANDLE_FILE_INFORMATION fi{};
  HANDLE handle = source->GetHandle();
  require(handle && handle != INVALID_HANDLE_VALUE &&
              GetFileInformationByHandle(handle, &fi),
          "file information");
  CloseHandle(handle);
  require(
      (((UINT64(fi.nFileSizeHigh) << 32) | fi.nFileSizeLow) >= offset + size),
      "file size");
  std::vector<unsigned char> out(expected.size() + 2, 0xa5);
  Request r{1,
            out.data() + 1,
            0,
            static_cast<UINT32>(expected.size()),
            source.Get(),
            offset,
            size,
            0,
            17,
            nullptr};
  q->EnqueueRequest(&r);
  q->EnqueueStatus(status.Get(), 0);
  require(!status->IsComplete(0) && status->GetHResult(0) == E_PENDING,
          "status pending before submit");
  q->Submit();
  check(wait_status(status.Get(), 0), "zlib read");
  require(std::equal(expected.begin(), expected.end(), out.begin() + 1) &&
              out.front() == 0xa5 && out.back() == 0xa5,
          "zlib bytes or guards");
  if (argc >= 5) {
    std::printf("retail=%zu offset=%llu ", expected.size(), offset);
    return;
  }
  std::fill(out.begin(), out.end(), static_cast<unsigned char>(0xa5));
  r.options = 0;
  r.offset = 0;
  r.size = 16;
  q->EnqueueRequest(&r);
  q->EnqueueStatus(status.Get(), 1);
  q->Submit();
  check(wait_status(status.Get(), 1), "raw read");
  require(std::equal(expected.begin(), expected.end(), out.begin() + 1),
          "raw bytes");
  std::fill(out.begin(), out.end(), static_cast<unsigned char>(0xa5));
  q->EnqueueRequest(&r);
  q->CancelRequestsWithTag(~UINT64(0), 17);
  q->EnqueueStatus(status.Get(), 0);
  q->Submit();
  check(wait_status(status.Get(), 0), "cancelled read");
  require(
      std::all_of(out.begin(), out.end(), [](unsigned v) { return v == 0xa5; }),
      "cancelled write");
  // BCPack BC2 (bits 1-4 = 2) has no decoder: the request fails and leaves
  // its destination unwritten instead of copying packed bytes into a texture.
  r.options = UINT64(2) << 1;
  q->EnqueueRequest(&r);
  q->EnqueueStatus(status.Get(), 0);
  q->Submit();
  require(wait_status(status.Get(), 0) == E_NOTIMPL &&
              std::all_of(out.begin(), out.end(),
                          [](unsigned v) { return v == 0xa5; }),
          "BCPack refused");
  alignas(8) std::array<BYTE, 1024> record{};
  q->RetrieveErrorRecord(record.data());
  r.options = 0;
  HANDLE event = q->GetErrorEvent();
  require(event != nullptr, "error event");
  r.offset = 65536;
  q->EnqueueRequest(&r);
  q->EnqueueStatus(status.Get(), 0);
  r.options = 1;
  r.offset = 0;
  r.size = 16;
  q->EnqueueRequest(&r);
  q->EnqueueStatus(status.Get(), 1);
  r.options = 0;
  q->EnqueueRequest(&r);
  q->EnqueueStatus(status.Get(), 2);
  source.Reset();
  q->Submit();
  check(wait_status(status.Get(), 2), "next batch recovers after failure");
  require(FAILED(status->GetHResult(0)) && FAILED(status->GetHResult(1)),
          "EOF and corrupt zlib");
  require(WaitForSingleObject(event, 1000) == WAIT_OBJECT_0,
          "signalled error event");
  q->RetrieveErrorRecord(record.data());
  require(*reinterpret_cast<const UINT32 *>(record.data()) == 2,
          "two recorded errors");
  require(WaitForSingleObject(event, 0) == WAIT_TIMEOUT, "reset error event");
  q->RetrieveErrorRecord(record.data());
  UINT32 count, kind;
  std::memcpy(&count, record.data(), sizeof(count));
  std::memcpy(&kind, record.data() + 12, sizeof(kind));
  require(count == 0 && kind == UINT32_MAX, "cleared first failure");
  CloseHandle(event);
  check(f->OpenFile(file, IID_PPV_ARGS(&source)), "reopen file");
  std::fill(out.begin(), out.end(), static_cast<unsigned char>(0xa5));
  r.file = source.Get();
  q->EnqueueRequest(&r);
  q->Submit();
  source->Close();
  require(std::equal(expected.begin(), expected.end(), out.begin() + 1) &&
              !source->GetHandle(),
          "explicit close waits for outstanding read");

  ComPtr<Queue> memory;
  QueueDesc mem_desc{1, 0x80, 2, "Memory queue"};
  check(f->CreateQueue(&mem_desc, IID_PPV_ARGS(&memory)), "memory queue");
  std::fill(out.begin(), out.end(), static_cast<unsigned char>(0xa5));
  r.options = (UINT64(1) << 22) | 1;
  r.file =
      reinterpret_cast<IUnknown *>(const_cast<unsigned char *>(compressed));
  r.size = sizeof(compressed);
  memory->EnqueueRequest(&r);
  memory->EnqueueStatus(status.Get(), 2);
  memory->Submit();
  check(wait_status(status.Get(), 2), "memory zlib");
  require(std::equal(expected.begin(), expected.end(), out.begin() + 1) &&
              out.front() == 0xa5 && out.back() == 0xa5,
          "memory bytes and guards");
  // Four BC1 blocks packed by the codec's test encoder (see the guest
  // fixture in runtime/xcrdapi/codec/src/tests/bc1.rs), then the same stream
  // behind zlib with its size as the intermediate size.
  static const unsigned char bc1[] = {
      0x1d, 0x18, 0x1f, 0x1d, 0x1b, 0x10, 0x15, 0x13, 0x03, 0x03, 0x05,
      0x03, 0x03, 0x04, 0x03, 0x03, 0x17, 0x19, 0x01, 0x01, 0x02, 0x04,
      0x10, 0x0c, 0x8d, 0xd5, 0x00, 0x38, 0x8d, 0x01, 0x36, 0xf2, 0x36,
      0x00, 0xff, 0x98, 0x85, 0x00, 0x1c, 0x06, 0x3b, 0x24, 0xe1, 0x81,
      0x54, 0x02, 0x48, 0xd5, 0x00, 0x05, 0x18, 0x14, 0x1e, 0x15, 0x13,
      0x00, 0x11, 0x13, 0x1f, 0x04, 0x03, 0x05, 0x04, 0x19, 0x03, 0x03,
      0x05, 0x0f, 0x4c, 0x1c, 0x27, 0x03, 0x2d, 0x19, 0x01, 0xf9, 0xeb,
      0x36, 0x76, 0x02, 0x82, 0x70, 0x01, 0x02, 0x01, 0xf9, 0x02, 0xd0,
      0x04, 0x09, 0x20, 0x3e, 0xe5, 0x35, 0x7b, 0x56, 0xc1, 0x01, 0x02,
      0x0a, 0x00, 0x03, 0x51, 0x11, 0x3d, 0x04, 0xa6, 0xa9, 0x10, 0xf0,
      0x73, 0x7b, 0x5d, 0x00, 0xc5, 0x19, 0x13, 0x17, 0x1a, 0x1d, 0x1a,
      0x19, 0x14, 0x17, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x00,
      0x64, 0x06, 0x05, 0x1f, 0x02, 0x05, 0x4f, 0x1e, 0x83, 0x0a, 0x0b,
      0x72, 0x20, 0x13, 0x26, 0x03, 0x02, 0x53, 0x6b, 0xfb, 0x01, 0x30,
      0x07, 0x04, 0x5d, 0xdb, 0xa8, 0x8c, 0x01, 0x23, 0xb8, 0xfc, 0xdd,
      0x05, 0x43, 0x13, 0xcb, 0xff, 0xba, 0x0f, 0x24, 0x16, 0x15, 0x16,
      0x1d, 0x11, 0x18, 0x17, 0x00, 0x03, 0x03, 0x04, 0x05, 0x03, 0x04,
      0x04, 0x19, 0x3a, 0x3b, 0x01, 0x01, 0x1d, 0x09, 0x05, 0x04, 0x83,
      0x33, 0x20, 0x00, 0xca, 0xd8, 0x72, 0x40, 0x2c, 0x02, 0xf2, 0xff,
      0x86, 0xff, 0xff, 0x00, 0xa0, 0x78, 0x70, 0xe0, 0x50, 0xf6, 0xe3,
      0xff, 0x00, 0x00, 0x00, 0x00, 0xfc, 0x1d, 0x18, 0x18, 0x00, 0x00,
      0x05, 0x03, 0x03, 0x10, 0x01, 0x28, 0x28, 0x04, 0x00, 0x81, 0x81,
      0x01, 0xff, 0x59, 0x52, 0x00, 0x24, 0x04, 0xb2, 0x22, 0x00, 0x00,
      0x00, 0x00};
  static const unsigned char bc1_zlib[] = {
      0x78, 0xda, 0x93, 0x95, 0x90, 0x97, 0x95, 0x16, 0x10, 0x15, 0x66,
      0x66, 0x66, 0x65, 0x66, 0x66, 0x61, 0x66, 0x16, 0x97, 0x64, 0x64,
      0x64, 0x62, 0x11, 0xe0, 0xe9, 0xbd, 0xca, 0x60, 0xd1, 0xcb, 0x68,
      0xf6, 0xc9, 0x8c, 0xe1, 0xff, 0x8c, 0x56, 0x06, 0x19, 0x36, 0x6b,
      0x95, 0x87, 0x8d, 0x21, 0x4c, 0x1e, 0x57, 0x19, 0x58, 0x25, 0x44,
      0xe4, 0x44, 0x85, 0x19, 0x04, 0x85, 0xe5, 0x59, 0x98, 0x59, 0x59,
      0x24, 0x81, 0xfa, 0xf8, 0x7d, 0x64, 0xd4, 0x99, 0x75, 0x25, 0x19,
      0x7f, 0xbe, 0x36, 0x2b, 0x63, 0x6a, 0x2a, 0x60, 0x64, 0x62, 0xfc,
      0xc9, 0x74, 0x81, 0x85, 0x53, 0xc1, 0xee, 0xa9, 0x69, 0x75, 0xd8,
      0x41, 0x46, 0x26, 0x2e, 0x06, 0xe6, 0x40, 0x41, 0x5b, 0x96, 0x65,
      0x2b, 0x05, 0x3e, 0x14, 0x57, 0xc7, 0x32, 0x1c, 0x95, 0x14, 0x16,
      0x97, 0x92, 0x95, 0x92, 0x14, 0x11, 0x67, 0x86, 0x00, 0x86, 0x14,
      0x36, 0x56, 0x79, 0x26, 0x56, 0x7f, 0xb9, 0x66, 0x2e, 0xee, 0x22,
      0x05, 0x61, 0x35, 0x66, 0xa6, 0xe0, 0xec, 0xdf, 0x8c, 0x06, 0xec,
      0x2c, 0xb1, 0xb7, 0x57, 0xf4, 0x30, 0x2a, 0xef, 0xf8, 0x73, 0x97,
      0xd5, 0x59, 0xf8, 0xf4, 0xff, 0x5d, 0xfc, 0x2a, 0x62, 0xa2, 0x62,
      0xb2, 0x82, 0x12, 0xe2, 0x0c, 0x40, 0x97, 0xb2, 0x32, 0xb3, 0xb0,
      0x48, 0x5a, 0x59, 0x33, 0x32, 0xca, 0x72, 0xb2, 0xb2, 0x34, 0x1b,
      0x2b, 0x30, 0x9c, 0xba, 0x51, 0xe4, 0xa0, 0xc3, 0xf4, 0xe9, 0x7f,
      0xdb, 0xff, 0xff, 0x0c, 0x0b, 0x2a, 0x0a, 0x1e, 0x04, 0x7c, 0x7b,
      0xfc, 0x9f, 0x01, 0x08, 0xfe, 0xc8, 0x4a, 0x48, 0x30, 0x30, 0x00,
      0x3d, 0x27, 0xc0, 0xa8, 0xa1, 0xc1, 0xc2, 0xd0, 0xd8, 0xc8, 0xf8,
      0x3f, 0x32, 0x88, 0x41, 0x85, 0x65, 0x93, 0x12, 0x48, 0x16, 0x00,
      0x1a, 0x98, 0x3b, 0x33};
  static const unsigned char bc1_blocks[] = {
      0x00, 0xf8, 0xe0, 0x07, 0xaa, 0xaa, 0x55, 0x55, 0x00, 0xf8, 0xe0,
      0x07, 0x67, 0x45, 0x23,       0x01, 0xff, 0xff, 0x1f, 0x00, 0x00,
      0x00, 0xff, 0xff, 0x08, 0x42, 0x10, 0x84, 0xe4, 0xe4,       0x1b,
      0x1b};
  // Two BC7 mode-6 blocks from the same encoder (runtime/xcrdapi/codec/src/
  // tests/bc7.rs): the 16-lane path with a dictionary reference.
  static const unsigned char bc7[] = {
      0x1a, 0x11, 0x1d, 0x19, 0x1a, 0x16, 0x1e, 0x15, 0x03, 0x03, 0x05,
      0x04, 0x04, 0x04, 0x03, 0x04, 0x01, 0x0b, 0x01, 0x08, 0x40, 0xac,
      0x01, 0x03, 0x63, 0x92, 0x00, 0xd3, 0x00, 0xbf, 0x62, 0x79, 0x7f,
      0x01, 0xff, 0xf9, 0xfe, 0xb7, 0x46, 0x06, 0x41, 0xc1, 0xec, 0xb5,
      0x9c, 0x30, 0xe5, 0x97, 0x00, 0x00, 0x00, 0x00, 0x9d, 0x17, 0x15,
      0x12, 0x17, 0x12, 0x00, 0x1d, 0x10, 0x15, 0x03, 0x03, 0x03, 0x03,
      0x15, 0x05, 0x05, 0x04, 0x11, 0x3d, 0x0b, 0x4b, 0x05, 0x01, 0x01,
      0x0d, 0x8b, 0x5c, 0x63, 0x91, 0x02, 0x00, 0x00, 0xb0, 0x0d, 0x1d,
      0x18, 0x00, 0x74, 0xff, 0x5a, 0x02, 0x04, 0xe5, 0xa2, 0x9e, 0xf7,
      0x1f, 0x32, 0xf3, 0x04, 0x1c, 0xbc, 0x14, 0x04, 0x07, 0x16, 0x18,
      0x19, 0x13, 0x03, 0x69, 0x01, 0x04, 0x03, 0x05, 0x03, 0x26, 0x03,
      0x00, 0x09, 0x9e, 0x01, 0x42, 0xaa, 0x38, 0xff, 0x24, 0x4c, 0x00,
      0xbc, 0x00, 0x57, 0x77, 0xfd, 0xc7, 0xff, 0xd4, 0x6b, 0xff, 0x54,
      0x66, 0xd9, 0xd0, 0x58, 0x00, 0x00, 0x8f, 0x00, 0x00, 0x78, 0x00,
      0x19, 0x16, 0x00, 0xb0, 0x00, 0x1b, 0x12, 0x1c, 0x1f, 0x10, 0x00,
      0x1c, 0x04, 0x03, 0x03, 0x03, 0x04, 0x18, 0x03, 0x05, 0x07, 0x42,
      0x02, 0x01, 0x10, 0x04, 0x12, 0x01, 0x87, 0xdf, 0xeb, 0x00, 0x08,
      0x88, 0xbe, 0x00, 0xfc, 0x02, 0x21, 0x40, 0x00, 0x70, 0x0d, 0xff,
      0x66, 0x55, 0x71, 0x02, 0x09, 0xf8, 0x51, 0x55, 0x04, 0x03, 0x18,
      0x17, 0x1a, 0x7f, 0x04, 0x03, 0x04, 0x80, 0x55, 0x04, 0x40, 0xa1,
      0x55, 0xc7, 0x00, 0x9e, 0xfe, 0x1d, 0xfe, 0x84, 0x11, 0xbc, 0x19,
      0x00, 0x00, 0x00, 0x00, 0x11, 0x60, 0x00, 0x07, 0x15, 0x1f, 0x17,
      0x19, 0x00, 0x19, 0x17, 0x16, 0x1d, 0x03, 0x03, 0x05, 0x18, 0x03,
      0x04, 0x03, 0x03, 0x01, 0x04, 0x01, 0x03, 0x12, 0x01, 0x2d, 0x01,
      0x00, 0x44, 0x00, 0x9e, 0xbe, 0xf0, 0x7a, 0xbd, 0x10, 0x6b, 0xff,
      0x4c, 0x0d, 0xe5, 0x04, 0x86, 0x01, 0xab, 0xc4, 0xd4, 0x51, 0x1c,
      0x47, 0xe9, 0x56, 0x15, 0x1b, 0x01, 0x03, 0x16, 0x03, 0x05, 0x00,
      0x47, 0x04, 0x1d, 0x01, 0xff, 0xb3, 0x01, 0xd4, 0x00, 0x8a, 0x27,
      0x20, 0x03, 0xff, 0xff, 0x2c, 0xf5, 0x9d, 0x74, 0x91, 0xfc, 0x1d,
      0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x1a, 0x00, 0x00, 0x00, 0x01,
      0x03, 0x00, 0x02, 0xff, 0x20, 0x57, 0x01, 0x00, 0x01, 0x00};
  static const unsigned char bc7_blocks[] = {
      0xc0, 0x9d, 0xcd, 0x04, 0xe7, 0xb6, 0x3a, 0x2e, 0x8a, 0x61, 0xa9,
      0x00, 0xf3, 0xec, 0x59, 0x4f, 0xc0, 0xe3, 0x79, 0x7f, 0xad, 0xb7,
      0x64, 0x82, 0x5c, 0xf1, 0xae, 0x72, 0x0a, 0xd4, 0x1f, 0xf8};
  std::vector<unsigned char> blocks(sizeof(bc1_blocks) + 2);
  for (UINT64 zlib = 0; zlib < 2; ++zlib) {
    std::fill(blocks.begin(), blocks.end(), static_cast<unsigned char>(0xa5));
    Request b{(UINT64(1) << 22) | (UINT64(1) << 1) | zlib,
              blocks.data() + 1,
              0,
              sizeof(bc1_blocks),
              reinterpret_cast<IUnknown *>(
                  const_cast<unsigned char *>(zlib ? bc1_zlib : bc1)),
              0,
              zlib ? UINT32(sizeof(bc1_zlib)) : UINT32(sizeof(bc1)),
              zlib ? UINT32(sizeof(bc1)) : 0,
              0,
              nullptr};
    memory->EnqueueRequest(&b);
    memory->EnqueueStatus(status.Get(), 2);
    memory->Submit();
    check(wait_status(status.Get(), 2), zlib ? "zlib then BC1" : "BCPack BC1");
    require(std::equal(std::begin(bc1_blocks), std::end(bc1_blocks),
                       blocks.begin() + 1) &&
                blocks.front() == 0xa5 && blocks.back() == 0xa5,
            "BC1 blocks and guards");
  }
  {
    std::vector<unsigned char> b7(sizeof(bc7_blocks) + 2, 0xa5);
    Request b{(UINT64(1) << 22) | (UINT64(7) << 1),
              b7.data() + 1,
              0,
              sizeof(bc7_blocks),
              reinterpret_cast<IUnknown *>(const_cast<unsigned char *>(bc7)),
              0,
              sizeof(bc7),
              0,
              0,
              nullptr};
    memory->EnqueueRequest(&b);
    memory->EnqueueStatus(status.Get(), 2);
    memory->Submit();
    check(wait_status(status.Get(), 2), "BCPack BC7");
    require(std::equal(std::begin(bc7_blocks), std::end(bc7_blocks),
                       b7.begin() + 1) &&
                b7.front() == 0xa5 && b7.back() == 0xa5,
            "BC7 blocks and guards");
  }
  {
    // Physical-page destinations through xmem.dll. ProjectScarlett fills some
    // page arrays after EnqueueRequest and before Submit, so an entry still
    // zero at enqueue is read when the request runs. One seen at enqueue is
    // kept even if the title reuses the array.
    HMODULE xmem = LoadLibraryW(L"xmem.dll");
    require(xmem, "load xmem.dll");
    using Allocate = BOOL(WINAPI *)(UINT64, SIZE_T *, UINT64 *);
    using Map = void *(WINAPI *)(void *, SIZE_T, const UINT64 *);
    using Free = BOOL(WINAPI *)(SIZE_T, const UINT64 *);
    auto allocate = reinterpret_cast<Allocate>(
        GetProcAddress(xmem, "XMemAllocatePhysicalPages"));
    auto map = reinterpret_cast<Map>(GetProcAddress(xmem, "XMemMapPhysicalPages"));
    auto release = reinterpret_cast<Free>(GetProcAddress(xmem, "XMemFreePhysicalPages"));
    require(allocate && map && release, "xmem page exports");
    UINT64 tokens[2];
    SIZE_T pages = 2;
    require(allocate(0x20400000, &pages, tokens) && pages == 2, "allocate pages");
    const UINT64 to_pages = (UINT64(1) << 22) | (UINT64(1) << 21);
    UINT64 late[1] = {0}, early[1] = {tokens[1]};
    Request p{to_pages, late, 0x4000, 16,
              reinterpret_cast<IUnknown *>(expected.data()), 0, 16, 0, 0, nullptr};
    memory->EnqueueRequest(&p);
    late[0] = tokens[0];
    p.dest = early;
    p.pageOffset = 0x8000;
    memory->EnqueueRequest(&p);
    early[0] = 0;
    memory->EnqueueStatus(status.Get(), 2);
    memory->Submit();
    check(wait_status(status.Get(), 2), "page array filled after enqueue");
    auto *view = static_cast<unsigned char *>(
        VirtualAlloc(nullptr, 2 << 16, MEM_RESERVE, PAGE_NOACCESS));
    require(view && map(view, 2, tokens) == view, "map pages");
    require(std::equal(expected.begin(), expected.begin() + 16, view + 0x4000) &&
                std::equal(expected.begin(), expected.begin() + 16,
                           view + (1 << 16) + 0x8000),
            "page bytes");
    require(map(view, 2, nullptr) == view && VirtualFree(view, 0, MEM_RELEASE) &&
                release(2, tokens),
            "release pages");
  }
  {
    // A request the title cancels while it is already running completes
    // without counting as a failure. 64 MiB of stored deflate blocks with a
    // wrong Adler-32 keep the inflate busy long enough to cancel it.
    const size_t total = 64 << 20, block = 65535;
    std::vector<unsigned char> z{0x78, 0x01};
    for (size_t done = 0; done < total; done += block) {
      const UINT16 n = static_cast<UINT16>((std::min)(block, total - done));
      z.push_back(done + n == total ? 1 : 0);
      z.push_back(static_cast<unsigned char>(n));
      z.push_back(static_cast<unsigned char>(n >> 8));
      const UINT16 nlen = static_cast<UINT16>(~n);
      z.push_back(static_cast<unsigned char>(nlen));
      z.push_back(static_cast<unsigned char>(nlen >> 8));
      z.insert(z.end(), n, 0);
    }
    z.insert(z.end(), {0xde, 0xad, 0xbe, 0xef});
    std::vector<unsigned char> sink(total);
    Request c{(UINT64(1) << 22) | 1,
              sink.data(),
              0,
              UINT32(total),
              reinterpret_cast<IUnknown *>(z.data()),
              0,
              UINT32(z.size()),
              0,
              0x99,
              nullptr};
    ComPtr<Factory2> f2;
    check(f.As(&f2), "factory stats");
    memory->EnqueueRequest(&c);
    memory->EnqueueStatus(status.Get(), 2);
    memory->Submit();
    StorageStats stats{};
    ULONGLONG deadline = GetTickCount64() + 10000;
    do
      f2->GetStats(&stats);
    while (stats.priorities[3].pending == 0 && status->GetHResult(2) == E_PENDING &&
           GetTickCount64() < deadline);
    require(stats.priorities[3].pending == 1, "cancelled read in flight");
    memory->CancelRequestsWithTag(~UINT64(0), 0x99);
    require(wait_status(status.Get(), 2) == S_OK,
            "a read cancelled while running is not a failure");
  }
  r.capacity = 4;
  std::fill(out.begin(), out.end(), static_cast<unsigned char>(0xa5));
  memory->EnqueueRequest(&r);
  memory->EnqueueStatus(status.Get(), 2);
  memory->Close();
  require(FAILED(status->GetHResult(2)) &&
              std::all_of(out.begin(), out.end(),
                          [](unsigned v) { return v == 0xa5; }),
          "bounded inflate");

  // Fill exactly half the ring without calling Submit.
  for (unsigned i = 0; i < 63; ++i)
    q->EnqueueStatus(status.Get(), 0);
  q->EnqueueStatus(status.Get(), 1);
  check(wait_status(status.Get(), 1), "automatic submit");
  q->Close();
  require(DeleteFileW(file) != FALSE, "release input file");
}

int wmain(int argc, wchar_t **argv) {
  try {
    run(argc, argv);
    constexpr auto result =
        "raw=16 zlib=16 guards=2 errors=2 cancellation=2 bc1=4 bc7=2 pages=2 autosubmit=64 PASS";
    if (argc == 2 || argc == 6) {
      std::ofstream report(argv[argc - 1], std::ios::binary);
      if (argc == 6)
        report << "retail PASS";
      else
        report << result;
      require(report.good(), "write receipt");
    }
    std::puts(argc >= 5 ? "PASS" : result);
    return 0;
  } catch (const std::exception &e) {
    std::fprintf(stderr, "xcrdapi-read FAIL %s\n", e.what());
    return 1;
  }
}
