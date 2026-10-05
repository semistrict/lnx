//go:build darwin

package hv

import (
	"encoding/binary"
	"fmt"
	"os"
)

// ARM64 Image header (64 bytes at the start of a Linux kernel Image).
// Reference: Documentation/arch/arm64/booting.rst
type arm64Header struct {
	Code0      uint32 // Executable code
	Code1      uint32 // Executable code
	TextOffset uint64 // Image load offset (little-endian)
	ImageSize  uint64 // Effective Image size (little-endian)
	Flags      uint64 // Kernel flags
	Res2       uint64 // Reserved
	Res3       uint64 // Reserved
	Res4       uint64 // Reserved
	Magic      uint32 // 0x644d5241 ("ARM\x64")
	Res5       uint32 // Reserved (PE header offset)
}

const arm64Magic = 0x644d5241 // "ARM\x64"

// GuestLayout describes where everything is placed in guest physical memory.
type GuestLayout struct {
	DTBAddr     uint64 // GPA of DTB
	KernelAddr  uint64 // GPA of kernel entry point
	InitrdAddr  uint64 // GPA of initrd start
	InitrdEnd   uint64 // GPA of initrd end
	RAMBase     uint64
	RAMSize     uint64
}

// LoadKernel reads an ARM64 kernel Image and returns its contents and text offset.
func LoadKernel(path string) ([]byte, uint64, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, 0, fmt.Errorf("read kernel: %w", err)
	}
	if len(data) < 64 {
		return nil, 0, fmt.Errorf("kernel too small: %d bytes", len(data))
	}

	magic := binary.LittleEndian.Uint32(data[56:])
	if magic != arm64Magic {
		return nil, 0, fmt.Errorf("not an ARM64 Image (magic 0x%08x, want 0x%08x)", magic, arm64Magic)
	}

	textOffset := binary.LittleEndian.Uint64(data[8:])
	// Kernel 5.8+ may set text_offset to 0, meaning load at any 2MB-aligned offset.
	if textOffset == 0 {
		textOffset = 0 // load at ramBase directly
	}

	return data, textOffset, nil
}

// PlaceGuest lays out kernel, initrd, and DTB in guest RAM.
// Returns the GuestLayout and the DTB blob.
//
// Memory layout (matching QEMU ARM virt):
//   ramBase + 0:          DTB (up to 2MB)
//   ramBase + textOffset: Kernel Image
//   after kernel (aligned): Initrd
func PlaceGuest(mem []byte, ramBase uint64, kernelData []byte, textOffset uint64, initrdData, dtbData []byte) (*GuestLayout, error) {
	ramSize := uint64(len(mem))

	// DTB goes at the start of RAM.
	dtbAddr := ramBase
	dtbOff := uint64(0)
	if uint64(len(dtbData)) > 2<<20 {
		return nil, fmt.Errorf("DTB too large: %d bytes", len(dtbData))
	}
	copy(mem[dtbOff:], dtbData)

	// Kernel goes at ramBase + textOffset. If textOffset is 0, use 2MB alignment.
	kernelOff := textOffset
	if kernelOff == 0 {
		kernelOff = 2 << 20 // 2MB
	}
	if kernelOff+uint64(len(kernelData)) > ramSize {
		return nil, fmt.Errorf("kernel doesn't fit: offset 0x%x + size %d > ram %d",
			kernelOff, len(kernelData), ramSize)
	}
	copy(mem[kernelOff:], kernelData)
	kernelAddr := ramBase + kernelOff

	// Initrd goes after the kernel, page-aligned.
	layout := &GuestLayout{
		DTBAddr:    dtbAddr,
		KernelAddr: kernelAddr,
		RAMBase:    ramBase,
		RAMSize:    ramSize,
	}

	if len(initrdData) > 0 {
		// Place initrd well away from kernel — at 128MB or after kernel, whichever is larger.
		// This matches QEMU's strategy and avoids kernel BSS overlap.
		initrdOff := align(kernelOff+uint64(len(kernelData)), 2<<20) // 2MB aligned
		if initrdOff < 128<<20 {
			initrdOff = 128 << 20
		}
		if initrdOff+uint64(len(initrdData)) > ramSize {
			return nil, fmt.Errorf("initrd doesn't fit: offset 0x%x + size %d > ram %d",
				initrdOff, len(initrdData), ramSize)
		}
		copy(mem[initrdOff:], initrdData)
		layout.InitrdAddr = ramBase + initrdOff
		layout.InitrdEnd = layout.InitrdAddr + uint64(len(initrdData))
	}

	return layout, nil
}

func align(v, a uint64) uint64 {
	return (v + a - 1) &^ (a - 1)
}
