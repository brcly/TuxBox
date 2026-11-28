// Read a root SRV far past its address, as console shaders do.
ByteAddressBuffer source : register(t0);
RWByteAddressBuffer result : register(u0);
[numthreads(1, 1, 1)]
void main() {
    // The first page, its end, the next page, 80 KiB and 1 MiB past the address.
    result.Store4(0, source.Load4(0x0u));
    result.Store4(16, source.Load4(0xff00u));
    result.Store4(32, source.Load4(0x10000u));
    result.Store4(48, source.Load4(0x14000u));
    result.Store4(64, source.Load4(0x100000u));
}
