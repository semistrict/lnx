//go:build linux

package main

import (
	"fmt"
	"os"
	"strings"
	"syscall"

	"github.com/mdlayher/vsock"
)

func mountInitialFS() error {
	mounts := []struct {
		source, target, fstype string
		flags                  uintptr
	}{
		{"proc", "/proc", "proc", 0},
		{"sysfs", "/sys", "sysfs", 0},
		{"devtmpfs", "/dev", "devtmpfs", 0},
		{"tmpfs", "/dev/shm", "tmpfs", 0},
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

// mountCachedLower mounts a 9P share read-only as the lower layer for
// the FUSE lazy-cache. The FUSE server is started post-pivotRoot.
func mountCachedLower(guestPath, tag string, port uint32) error {
	lower := "/mnt/var/lnx/lower/" + tag
	cache := "/mnt/var/lnx/cache/" + tag

	os.MkdirAll(lower, 0755)
	if err := mount9P(lower, port, true); err != nil {
		return fmt.Errorf("mount 9p lower %s: %w", tag, err)
	}
	os.MkdirAll(cache, 0755)
	os.MkdirAll("/mnt"+guestPath, 0755)
	return nil
}

// mountDirect mounts a 9P share directly at the guest path (no FUSE cache).
func mountDirect(guestPath string, port uint32, readOnly bool) error {
	target := "/mnt" + guestPath
	os.MkdirAll(target, 0755)
	return mount9P(target, port, readOnly)
}

// mount9P dials a 9P server on the host via vsock and mounts it at target.
func mount9P(target string, port uint32, readOnly bool) error {
	conn, err := vsock.Dial(vsockHostCID, port, nil)
	if err != nil {
		return fmt.Errorf("vsock dial 9p port %d: %w", port, err)
	}

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
	conn.Close()
	if dupErr != nil {
		return fmt.Errorf("9p dup fd: %w", dupErr)
	}

	var flags uintptr
	if readOnly {
		flags = syscall.MS_RDONLY
	}
	opts := fmt.Sprintf("trans=fd,rfdno=%d,wfdno=%d,version=9p2000.L,msize=1048576", fd, fd)
	if err := syscall.Mount("9p", target, "9p", flags, opts); err != nil {
		syscall.Close(fd)
		return fmt.Errorf("mount 9p on %s (port %d): %w", target, port, err)
	}
	return nil
}


func mountCgroups() error {
	os.MkdirAll("/sys/fs/cgroup", 0755)
	// Use cgroup v1 hybrid: tmpfs base with individual controllers.
	// Docker requires the devices cgroup which doesn't exist in pure cgroup v2.
	if err := syscall.Mount("tmpfs", "/sys/fs/cgroup", "tmpfs", 0, ""); err != nil {
		return fmt.Errorf("mount cgroup tmpfs: %w", err)
	}
	controllers := []string{"cpu,cpuacct", "memory", "devices", "freezer", "pids", "blkio", "cpuset", "net_cls,net_prio", "perf_event", "hugetlb"}
	for _, c := range controllers {
		name := strings.Split(c, ",")[0] // use first name for dir
		dir := "/sys/fs/cgroup/" + name
		os.MkdirAll(dir, 0755)
		syscall.Mount("cgroup", dir, "cgroup", 0, c)
	}
	// Mount cgroup2 for unified hierarchy support.
	os.MkdirAll("/sys/fs/cgroup/unified", 0755)
	syscall.Mount("cgroup2", "/sys/fs/cgroup/unified", "cgroup2", 0, "")
	return nil
}

func mountInNewRoot() error {
	for _, m := range []struct{ src, dst, fstype string }{
		{"/proc", "/mnt/proc", "proc"},
		{"/sys", "/mnt/sys", "sysfs"},
		{"/dev", "/mnt/dev", "devtmpfs"},
		{"/dev/shm", "/mnt/dev/shm", "tmpfs"},
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
