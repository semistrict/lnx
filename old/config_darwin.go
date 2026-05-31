//go:build darwin

package lnx

import (
	"encoding/binary"
	"syscall"
)

func hostMemoryBytes() uint64 {
	val, err := syscall.Sysctl("hw.memsize")
	if err != nil || len(val) < 8 {
		return 4 << 30 // fallback: 4 GiB
	}
	return binary.LittleEndian.Uint64([]byte(val[:8]))
}
