//go:build linux

package main

import (
	"encoding/json"
	"fmt"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"
)

const (
	// criuDevice is the block device for CRIU images (vdc).
	criuDevice = "/dev/vdc"
	// criuMountPoint is where the CRIU device is mounted.
	criuMountPoint = "/mnt/criu"
	// criuForkDir is the subdirectory used for fork dumps.
	criuForkDir = "/mnt/criu/fork"
	// forkRolePath is written by the child after a fork restore.
	forkRolePath = "/var/run/lnx/fork-role"
)

// criuCheckpointMetadata is written alongside CRIU image dirs so we
// know what was dumped.
type criuCheckpointMetadata struct {
	Name      string    `json:"name"`
	PIDs      []int     `json:"pids"`
	Timestamp time.Time `json:"timestamp"`
}

// mountCRIUDevice mounts the CRIU block device. If the device has no
// filesystem (first boot), it formats it with ext4 first.
func mountCRIUDevice() {
	if _, err := os.Stat(criuDevice); err != nil {
		slog.Debug("no CRIU device, skipping", "device", criuDevice)
		return
	}

	os.MkdirAll(criuMountPoint, 0755)

	// Try mounting first (preserves existing data from checkpoint restore).
	if err := syscall.Mount(criuDevice, criuMountPoint, "ext4", syscall.MS_NOATIME, "errors=continue"); err == nil {
		slog.Info("mounted CRIU device", "device", criuDevice, "target", criuMountPoint)
		return
	}

	// Not formatted yet — format and mount.
	slog.Info("formatting CRIU device", "device", criuDevice)
	if out, err := exec.Command("/sbin/mke2fs", "-t", "ext4", "-q", criuDevice).CombinedOutput(); err != nil {
		slog.Warn("mke2fs CRIU device failed", "error", err, "output", string(out))
		return
	}

	if err := syscall.Mount(criuDevice, criuMountPoint, "ext4", syscall.MS_NOATIME, "errors=continue"); err != nil {
		slog.Warn("mount CRIU device failed after format", "error", err)
		return
	}
	slog.Info("formatted and mounted CRIU device", "device", criuDevice, "target", criuMountPoint)
}

// criuDump dumps each tracked user process tree with CRIU.
// Each PID gets its own sub-directory under dir.
// If leaveRunning is true, processes continue after the dump.
func criuDump(name, dir string, leaveRunning bool) error {
	pids := listUserPIDs()
	if len(pids) == 0 {
		return fmt.Errorf("no user processes to checkpoint")
	}

	if err := os.MkdirAll(dir, 0755); err != nil {
		return fmt.Errorf("create CRIU dir: %w", err)
	}

	var dumpedPIDs []int
	for _, pid := range pids {
		pidDir := filepath.Join(dir, strconv.Itoa(pid))
		if err := os.MkdirAll(pidDir, 0755); err != nil {
			return fmt.Errorf("create PID dir %d: %w", pid, err)
		}

		// Find unsupported sockets (vsock etc) and externalize them
		// so CRIU drops them during dump instead of failing.
		externals := findUnsupportedSocketInodes(pid)

		args := []string{
			"dump",
			"--tree", strconv.Itoa(pid),
			"--images-dir", pidDir,
			"--shell-job",
			"--tcp-established",
			"--ext-unix-sk",
		}
		for _, inode := range externals {
			args = append(args, "--external", "socket["+inode+"]")
		}
		if leaveRunning {
			args = append(args, "--leave-running")
		}

		slog.Info("criu dump", "pid", pid, "dir", pidDir, "leaveRunning", leaveRunning,
			"externals", len(externals))
		cmd := exec.Command("criu", args...)
		output, err := cmd.CombinedOutput()
		if err != nil {
			// If this process can't be dumped (e.g. unsupported socket
			// types that --external can't handle), skip it and try the
			// rest. This happens for exec session shells that inherit
			// vsock FDs from init.
			slog.Warn("criu dump failed, skipping", "pid", pid, "error", err,
				"output", string(output))
			os.RemoveAll(pidDir)
			continue
		}
		dumpedPIDs = append(dumpedPIDs, pid)
	}

	if len(dumpedPIDs) == 0 {
		return fmt.Errorf("no user processes were dumped")
	}

	// Write metadata.
	meta := criuCheckpointMetadata{
		Name:      name,
		PIDs:      dumpedPIDs,
		Timestamp: time.Now(),
	}
	metaData, err := json.Marshal(meta)
	if err != nil {
		return fmt.Errorf("marshal metadata: %w", err)
	}
	if err := os.WriteFile(filepath.Join(dir, "metadata.json"), metaData, 0644); err != nil {
		return fmt.Errorf("write metadata: %w", err)
	}

	return nil
}

// findUnsupportedSocketInodes returns the inodes of sockets in the
// process tree that CRIU can't handle natively (e.g. vsock).
// These should be passed as --external socket[inode] to CRIU dump.
func findUnsupportedSocketInodes(pid int) []string {
	// Walk the process tree rooted at pid.
	var allPids []int
	allPids = append(allPids, pid)
	// Also check children (CRIU dumps the whole tree).
	if entries, err := os.ReadDir(fmt.Sprintf("/proc/%d/task/%d/children", pid, pid)); err == nil {
		for _, e := range entries {
			if childPid, err := strconv.Atoi(e.Name()); err == nil {
				allPids = append(allPids, childPid)
			}
		}
	}
	// Simpler: read /proc/<pid>/task/<tid>/children file.
	if data, err := os.ReadFile(fmt.Sprintf("/proc/%d/task/%d/children", pid, pid)); err == nil {
		for _, f := range strings.Fields(string(data)) {
			if childPid, err := strconv.Atoi(f); err == nil {
				allPids = append(allPids, childPid)
			}
		}
	}

	// Collect known socket inodes across all pids.
	knownInodes := make(map[string]bool)
	for _, p := range allPids {
		for k, v := range collectKnownSocketInodes(p) {
			knownInodes[k] = v
		}
	}

	// Find socket inodes that aren't in the known set.
	seen := make(map[string]bool)
	var inodes []string
	for _, p := range allPids {
		fdDir := fmt.Sprintf("/proc/%d/fd", p)
		entries, err := os.ReadDir(fdDir)
		if err != nil {
			continue
		}
		for _, e := range entries {
			link, err := os.Readlink(filepath.Join(fdDir, e.Name()))
			if err != nil || !strings.HasPrefix(link, "socket:[") {
				continue
			}
			inode := strings.TrimSuffix(strings.TrimPrefix(link, "socket:["), "]")
			if !knownInodes[inode] && !seen[inode] {
				seen[inode] = true
				inodes = append(inodes, inode)
			}
		}
	}
	return inodes
}

// collectKnownSocketInodes reads /proc/<pid>/net/{tcp,tcp6,udp,udp6,unix}
// and returns a set of socket inodes that CRIU can handle natively.
func collectKnownSocketInodes(pid int) map[string]bool {
	known := make(map[string]bool)
	netDir := fmt.Sprintf("/proc/%d/net", pid)

	for _, name := range []string{"tcp", "tcp6", "udp", "udp6", "unix"} {
		data, err := os.ReadFile(filepath.Join(netDir, name))
		if err != nil {
			continue
		}
		for _, line := range strings.Split(string(data), "\n") {
			fields := strings.Fields(line)
			if len(fields) < 10 {
				continue
			}
			// For tcp/udp: inode is field 9 (0-indexed).
			// For unix: inode is field 6.
			var inode string
			if name == "unix" {
				if len(fields) >= 7 {
					inode = fields[6]
				}
			} else {
				inode = fields[9]
			}
			if inode != "" && inode != "0" {
				known[inode] = true
			}
		}
	}
	return known
}

// criuRestore restores processes from CRIU images in dir.
// Each sub-directory should contain a CRIU image set for one process tree.
func criuRestore(dir string) error {
	metaPath := filepath.Join(dir, "metadata.json")
	data, err := os.ReadFile(metaPath)
	if err != nil {
		return fmt.Errorf("read metadata: %w", err)
	}

	var meta criuCheckpointMetadata
	if err := json.Unmarshal(data, &meta); err != nil {
		return fmt.Errorf("parse metadata: %w", err)
	}

	for _, pid := range meta.PIDs {
		pidDir := filepath.Join(dir, strconv.Itoa(pid))
		if _, err := os.Stat(pidDir); err != nil {
			slog.Warn("criu restore: PID dir missing, skipping", "pid", pid, "dir", pidDir)
			continue
		}

		args := []string{
			"restore",
			"--images-dir", pidDir,
			"--shell-job",
			"--restore-detached",
			"--tcp-established",
			"--ext-unix-sk",
		}

		slog.Info("criu restore", "pid", pid, "dir", pidDir)
		cmd := exec.Command("criu", args...)
		output, err := cmd.CombinedOutput()
		if err != nil {
			return fmt.Errorf("criu restore pid %d: %w\n%s", pid, err, string(output))
		}
	}

	return nil
}

// criuAutoRestore detects CRIU images on the CRIU volume from a fork or
// checkpoint restore and restores the processes automatically.
// Fork detection takes priority over checkpoint restore.
func criuAutoRestore() {
	// Check for fork images first.
	forkMeta := filepath.Join(criuForkDir, "metadata.json")
	if _, err := os.Stat(forkMeta); err == nil {
		slog.Info("detected CRIU fork images, restoring as child")
		os.MkdirAll(filepath.Dir(forkRolePath), 0755)
		os.WriteFile(forkRolePath, []byte("child\n"), 0644)

		if err := criuRestore(criuForkDir); err != nil {
			slog.Error("CRIU fork restore failed", "error", err)
		} else {
			slog.Info("CRIU fork restore complete")
		}
		os.RemoveAll(criuForkDir)
		return
	}

	// Check for checkpoint images (any subdirectory with metadata.json).
	entries, err := os.ReadDir(criuMountPoint)
	if err != nil {
		return
	}
	for _, e := range entries {
		if !e.IsDir() || e.Name() == "lost+found" {
			continue
		}
		dir := filepath.Join(criuMountPoint, e.Name())
		metaPath := filepath.Join(dir, "metadata.json")
		if _, err := os.Stat(metaPath); err != nil {
			continue
		}

		slog.Info("detected CRIU checkpoint images, restoring", "name", e.Name())
		if err := criuRestore(dir); err != nil {
			slog.Error("CRIU checkpoint restore failed", "name", e.Name(), "error", err)
		} else {
			slog.Info("CRIU checkpoint restore complete", "name", e.Name())
		}
		os.RemoveAll(dir)
		return // only restore the first one
	}
}

// installForkRoleHelper writes a script that returns the fork role
// ("parent", "child", or exits 1 if not in a fork).
func installForkRoleHelper() {
	script := `#!/bin/sh
if [ -f ` + forkRolePath + ` ]; then
    cat ` + forkRolePath + `
else
    echo "not a fork"
    exit 1
fi
`
	if err := os.WriteFile("/usr/local/bin/lnx-fork-role", []byte(script), 0755); err != nil {
		slog.Warn("failed to install lnx-fork-role helper", "error", err)
	}
}
