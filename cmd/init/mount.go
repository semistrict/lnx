//go:build linux

package main

import (
	"fmt"
	"os"
	"syscall"

	"github.com/mdlayher/vsock"
	"github.com/semistrict/lnx/internal/protocol"
)

func mountInitialFS() error {
	mounts := []struct {
		source, target, fstype string
		flags                  uintptr
	}{
		{"proc", "/proc", "proc", 0},
		{"sysfs", "/sys", "sysfs", 0},
		{"devtmpfs", "/dev", "devtmpfs", 0},
		{"tmpfs", "/tmp", "tmpfs", 0},
		{"tmpfs", "/run", "tmpfs", 0},
	}
	for _, m := range mounts {
		os.MkdirAll(m.target, 0755)
		if err := syscall.Mount(m.source, m.target, m.fstype, m.flags, ""); err != nil {
			return fmt.Errorf("mount %s on %s: %w", m.fstype, m.target, err)
		}
	}
	os.Symlink("/proc/self/fd", "/dev/fd")
	return nil
}

func mountRootfs() error {
	os.MkdirAll("/mnt", 0755)
	// noatime for performance; errors=continue to avoid panic on journal issues.
	if err := syscall.Mount("/dev/vda", "/mnt", "ext4", syscall.MS_NOATIME, "errors=continue"); err != nil {
		return fmt.Errorf("mount rootfs: %w", err)
	}
	return nil
}

// mountHome mounts the host home directory via 9P over vsock.
// The guest dials the host's 9P server, duplicates the fd (to take it
// out of Go's runtime poller), and passes it to the kernel 9p mount.
func mountHome(homeDir string) error {
	conn, err := vsock.Dial(vsockHostCID, protocol.P9Port, nil)
	if err != nil {
		return fmt.Errorf("vsock dial 9p: %w", err)
	}

	// Dup the fd so the kernel owns a copy independent of Go's poller.
	rawConn, err := conn.SyscallConn()
	if err != nil {
		conn.Close()
		return fmt.Errorf("9p syscall conn: %w", err)
	}

	var fd int
	var dupErr error
	rawConn.Control(func(f uintptr) {
		fd, dupErr = syscall.Dup(int(f))
	})
	// Close the original Go-managed connection now that we have a dup.
	conn.Close()
	if dupErr != nil {
		return fmt.Errorf("9p dup fd: %w", dupErr)
	}

	target := "/mnt" + homeDir
	os.MkdirAll(target, 0755)

	opts := fmt.Sprintf("trans=fd,rfdno=%d,wfdno=%d,version=9p2000.L,msize=1048576", fd, fd)
	if err := syscall.Mount("home", target, "9p", syscall.MS_RDONLY, opts); err != nil {
		syscall.Close(fd)
		return fmt.Errorf("mount home 9p on %s: %w", target, err)
	}

	// Don't close fd — the kernel holds it for the mount.
	return nil
}

func mountCWD(cwdPath string) error {
	target := "/mnt" + cwdPath
	os.MkdirAll(target, 0755)
	if err := syscall.Mount("cwd", target, "virtiofs", 0, ""); err != nil {
		return fmt.Errorf("mount virtiofs on %s: %w", target, err)
	}
	return nil
}

func mountInNewRoot() error {
	for _, m := range []struct{ src, dst, fstype string }{
		{"/proc", "/mnt/proc", "proc"},
		{"/sys", "/mnt/sys", "sysfs"},
		{"/dev", "/mnt/dev", "devtmpfs"},
		{"/tmp", "/mnt/tmp", "tmpfs"},
		{"/run", "/mnt/run", "tmpfs"},
	} {
		os.MkdirAll(m.dst, 0755)
		if err := syscall.Mount(m.src, m.dst, m.fstype, 0, ""); err != nil {
			return fmt.Errorf("mount %s in newroot: %w", m.dst, err)
		}
	}
	os.MkdirAll("/mnt/dev/pts", 0755)
	syscall.Mount("devpts", "/mnt/dev/pts", "devpts", 0, "newinstance,ptmxmode=0666")
	os.Remove("/mnt/dev/ptmx")
	os.Symlink("pts/ptmx", "/mnt/dev/ptmx")
	return nil
}

func pivotRoot() error {
	os.MkdirAll("/mnt/oldroot", 0755)
	if err := syscall.PivotRoot("/mnt", "/mnt/oldroot"); err != nil {
		return fmt.Errorf("pivot_root: %w", err)
	}
	if err := os.Chdir("/"); err != nil {
		return fmt.Errorf("chdir /: %w", err)
	}
	syscall.Unmount("/oldroot", syscall.MNT_DETACH)
	return nil
}
