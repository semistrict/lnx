package lnx

import (
	"fmt"
	"os"
	"strconv"
	"strings"
	"syscall"
)

type lockFile struct {
	lockFd  *os.File
	pidPath string
}

// lockRootfs takes an exclusive flock on a .lock file next to the rootfs.
// If a stale lock file exists from a crashed process, the flock will
// succeed (kernel releases flocks on process death) and we clean up.
func lockRootfs(rootfsPath string) (*lockFile, error) {
	lockPath := rootfsPath + ".lock"
	pidPath := rootfsPath + ".pid"

	f, err := os.OpenFile(lockPath, os.O_CREATE|os.O_RDWR, 0644)
	if err != nil {
		return nil, fmt.Errorf("open lock file: %w", err)
	}

	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		f.Close()
		msg := "rootfs is locked by another instance"
		if pid := readPidFile(pidPath); pid > 0 {
			msg += fmt.Sprintf(" (pid %d)", pid)
		}
		return nil, fmt.Errorf("%s", msg)
	}

	// We hold the lock. Clean up any stale pidfile from a crashed process.
	os.Remove(pidPath)

	if err := os.WriteFile(pidPath, []byte(fmt.Sprintf("%d\n", os.Getpid())), 0644); err != nil {
		f.Close()
		return nil, fmt.Errorf("write pidfile: %w", err)
	}

	return &lockFile{lockFd: f, pidPath: pidPath}, nil
}

func (l *lockFile) unlock() {
	if l == nil {
		return
	}
	lockPath := l.lockFd.Name()
	syscall.Flock(int(l.lockFd.Fd()), syscall.LOCK_UN)
	l.lockFd.Close()
	os.Remove(lockPath)
	os.Remove(l.pidPath)
}

func readPidFile(path string) int {
	data, err := os.ReadFile(path)
	if err != nil {
		return 0
	}
	pid, err := strconv.Atoi(strings.TrimSpace(string(data)))
	if err != nil {
		return 0
	}
	return pid
}
