package lnx

import (
	"encoding/binary"
	"path/filepath"
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

	// Root runs guest commands as root instead of mirroring the host user.
	// When true, no host home directory is mounted into the guest.
	Root bool

	// Checkpoint clones the rootfs before starting the VM.
	// The clone is stored under CheckpointDir with a timestamped name.
	// Requires APFS (macOS).
	Checkpoint bool

	// CheckpointDir is where checkpoint clones are stored.
	// Defaults to ~/.lnx/checkpoints/.
	CheckpointDir string

	// Shares is a list of extra host directories to share read-write via virtiofs.
	// Each path is mounted in the guest at the same absolute path.
	Shares []string

	// Hostname is the guest hostname. Defaults to "lnx".
	Hostname string

	// SSHAgent forwards the host's SSH agent into the guest.
	// Requires SSH_AUTH_SOCK to be set on the host.
	SSHAgent bool

	// Ephemeral clones the rootfs to a temp file via APFS clonefile
	// before booting. The clone is deleted on exit. The original rootfs
	// is never locked, so multiple ephemeral VMs can run concurrently.
	Ephemeral bool

	// GUI starts a headless Wayland desktop with VNC, accessible via noVNC in the browser.
	GUI bool

	// InitialHoldID keeps the daemon alive until the holder releases it.
	// Used by background GUI sessions so the VM stays up while the window is open.
	InitialHoldID string

	// SocketDir overrides the directory for status.sock.
	// If empty, defaults to the directory containing RootfsPath.
	// Useful for ephemeral mode where the rootfs is in a temp dir but
	// the socket must be in the instance dir for clients to find it.
	SocketDir string
}

func (c *Config) socketDir() string {
	if c.SocketDir != "" {
		return c.SocketDir
	}
	return filepath.Dir(c.RootfsPath)
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
