// This file is compiled by CGo on macOS to create a placeholder __LNX,__lnxpack
// section in the lnx binary. lnx pack resizes this section with compressed
// kernel+rootfs data using Mach-O section injection.
//
// Layout when packed:
//   [u64 data_size][zstd kernel][zstd rootfs][JSON config][u64 json_len]
//   [zero padding to 16KB alignment]
//
// When unpacked (build time), data_size is 0.

#include <stdint.h>

__attribute__((section("__LNX,__lnxpack"), used, aligned(16384)))
static uint8_t _lnx_pack_data[16384] = {0};
