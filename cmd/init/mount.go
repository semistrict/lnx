//go:build linux

package main

import (
	"fmt"
	"os"
	"syscall"
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

func mountHome(homeDir string) error {
	target := "/mnt" + homeDir
	os.MkdirAll(target, 0755)
	if err := syscall.Mount("home", target, "virtiofs", syscall.MS_RDONLY, ""); err != nil {
		return fmt.Errorf("mount home virtiofs on %s: %w", target, err)
	}
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
