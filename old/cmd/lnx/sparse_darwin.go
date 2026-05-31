//go:build darwin

package main

import (
	"os"
	"unsafe"

	"golang.org/x/sys/unix"
)

// fpunchhole is the struct passed to fcntl(F_PUNCHHOLE) on macOS.
type fpunchhole struct {
	Flags    uint32 // unused
	Reserved uint32 // alignment padding
	Offset   int64  // start of the region
	Length   int64  // size of the region
}

// punchHoles scans a file for zero-filled blocks and punches holes in them
// using fcntl(F_PUNCHHOLE). This reclaims physical disk space for regions
// that are all zeros, turning the file into a sparse file on APFS.
func punchHoles(path string, blockSize int) error {
	f, err := os.OpenFile(path, os.O_RDWR, 0)
	if err != nil {
		return err
	}
	defer f.Close()

	info, err := f.Stat()
	if err != nil {
		return err
	}
	size := info.Size()

	buf := make([]byte, blockSize)
	var punched int64

	for off := int64(0); off < size; off += int64(blockSize) {
		n, err := f.ReadAt(buf, off)
		if n == 0 && err != nil {
			break
		}

		if isZero(buf[:n]) {
			ph := fpunchhole{
				Offset: off,
				Length: int64(n),
			}
			_, _, errno := unix.Syscall(unix.SYS_FCNTL, f.Fd(), unix.F_PUNCHHOLE, uintptr(unsafe.Pointer(&ph)))
			if errno != 0 {
				// Not all filesystems support punchhole; stop trying.
				return nil
			}
			punched += int64(n)
		}
	}

	return nil
}

func isZero(b []byte) bool {
	for _, v := range b {
		if v != 0 {
			return false
		}
	}
	return true
}
