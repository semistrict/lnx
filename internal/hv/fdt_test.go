//go:build darwin

package hv

import (
	"encoding/binary"
	"testing"
)

func TestFDT_Magic(t *testing.T) {
	f := NewFDT()
	f.BeginNode("")
	f.PropStr("compatible", "test")
	f.EndNode()
	blob := f.Finish()

	if len(blob) < 40 {
		t.Fatalf("blob too small: %d bytes", len(blob))
	}
	magic := binary.BigEndian.Uint32(blob[0:])
	if magic != fdtMagic {
		t.Fatalf("expected magic 0x%08x, got 0x%08x", fdtMagic, magic)
	}
	totalSize := binary.BigEndian.Uint32(blob[4:])
	if totalSize != uint32(len(blob)) {
		t.Fatalf("header totalsize %d != actual %d", totalSize, len(blob))
	}
}

func TestBuildVirtDTB(t *testing.T) {
	blob := BuildVirtDTB(VirtDTBConfig{
		RAMBase:       0x4000_0000,
		RAMSize:       512 << 20,
		CPUs:          2,
		Cmdline:       "console=hvc0",
		GICDistBase:   0x0800_0000,
		GICDistSize:   0x10000,
		GICRedistBase: 0x080A_0000,
		GICRedistSize: 0xF6_0000,
		VirtioBase:    0x0A00_0000,
		VirtioStride:  0x200,
		VirtioCount:   4,
		VirtioIRQBase: 16,
	})

	magic := binary.BigEndian.Uint32(blob[0:])
	if magic != fdtMagic {
		t.Fatalf("bad magic: 0x%08x", magic)
	}
	t.Logf("DTB size: %d bytes", len(blob))

	// Verify the blob contains expected strings.
	s := string(blob)
	for _, want := range []string{
		"linux,dummy-virt",
		"arm,gic-v3",
		"arm,armv8-timer",
		"virtio,mmio",
		"console=hvc0",
		"arm,psci-1.0",
		"memory@",
		"cpu@0",
		"cpu@1",
	} {
		found := false
		for i := 0; i < len(s)-len(want); i++ {
			if s[i:i+len(want)] == want {
				found = true
				break
			}
		}
		if !found {
			t.Errorf("DTB missing string %q", want)
		}
	}
}
