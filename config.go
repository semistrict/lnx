package lnx

import (
	"encoding/binary"
	"syscall"
)

// Config holds the configuration for a VM instance.
type Config struct {
	// KernelPath is the path to the Linux kernel Image.
	KernelPath string

	// RootfsPath is the path to the ext4 rootfs image.
	RootfsPath string

	// InitramfsPath is where the generated initramfs cpio will be written.
	// If empty, defaults to the same directory as KernelPath.
	InitramfsPath string

	// CPUs is the number of virtual CPUs. Defaults to 2.
	CPUs uint

	// MemoryBytes is the amount of RAM in bytes.
	// Defaults to 50% of host physical memory.
	MemoryBytes uint64

	// CWD is the host directory to mount read-write inside the VM
	// at the same path via virtiofs. Defaults to os.Getwd().
	CWD string

	// Env is a list of extra KEY=VALUE environment variables to pass
	// to the guest, merged with the filtered host environment.
	Env []string

	// Checkpoint clones the rootfs before starting the VM.
	// The clone is stored under CheckpointDir with a timestamped name.
	// Requires APFS (macOS).
	Checkpoint bool

	// CheckpointDir is where checkpoint clones are stored.
	// Defaults to ~/.lnx/checkpoints/.
	CheckpointDir string
}

func (c *Config) cpus() uint {
	if c.CPUs == 0 {
		return 2
	}
	return c.CPUs
}

func (c *Config) memoryBytes() uint64 {
	if c.MemoryBytes == 0 {
		return hostMemoryBytes() / 2
	}
	return c.MemoryBytes
}

func hostMemoryBytes() uint64 {
	val, err := syscall.Sysctl("hw.memsize")
	if err != nil || len(val) < 8 {
		return 4 << 30 // fallback: 4 GiB
	}
	return binary.LittleEndian.Uint64([]byte(val[:8]))
}
