//go:build linux

package main

import (
	"os"

	"golang.org/x/sys/unix"
)

// punchHoles scans a file for zero-filled blocks and punches holes using
// fallocate(FALLOC_FL_PUNCH_HOLE). This reclaims physical disk space.
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

	for off := int64(0); off < size; off += int64(blockSize) {
		n, err := f.ReadAt(buf, off)
		if n == 0 && err != nil {
			break
		}

		if isZero(buf[:n]) {
			err := unix.Fallocate(int(f.Fd()), unix.FALLOC_FL_PUNCH_HOLE|unix.FALLOC_FL_KEEP_SIZE, off, int64(n))
			if err != nil {
				return nil // filesystem doesn't support it
			}
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
