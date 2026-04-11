//go:build linux

package main

import (
	"encoding/json"
	"fmt"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
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
	Name       string           `json:"name"`
	PIDs       []int            `json:"pids"`
	Timestamp  time.Time        `json:"timestamp"`
	PipeInodes map[int][]string `json:"pipe_inodes,omitempty"` // PID → external pipe inodes
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
	} else {
		slog.Warn("CRIU device mount failed, will format", "error", err)
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

// syncCRIUVolume forces all dirty data on the CRIU filesystem to disk.
func syncCRIUVolume() {
	syscall.Sync()
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
	pipeInodesMap := make(map[int][]string)
	var lastDumpErr string

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

		// Find pipes shared with init (fork pipes etc.) and externalize
		// them so CRIU doesn't try to checkpoint cross-boundary pipes.
		pipeInodes := findExternalPipeInodes(pid)
		for _, inode := range pipeInodes {
			args = append(args, "--external", "pipe["+inode+"]")
		}

		if leaveRunning {
			args = append(args, "--leave-running")
		}

		slog.Info("criu dump", "pid", pid, "dir", pidDir, "leaveRunning", leaveRunning,
			"externals", len(externals), "pipes", len(pipeInodes))
		cmd := exec.Command("criu", args...)
		output, err := cmd.CombinedOutput()
		if err != nil {
			// If this process can't be dumped (e.g. unsupported socket
			// types that --external can't handle), skip it and try the
			// rest. This happens for exec session shells that inherit
			// vsock FDs from init.
			lastDumpErr = fmt.Sprintf("pid %d: %v: %s", pid, err, string(output))
			slog.Warn("criu dump failed, skipping", "pid", pid, "error", err,
				"output", string(output))
			os.RemoveAll(pidDir)
			continue
		}
		dumpedPIDs = append(dumpedPIDs, pid)
		if len(pipeInodes) > 0 {
			pipeInodesMap[pid] = pipeInodes
		}
	}

	if len(dumpedPIDs) == 0 {
		if lastDumpErr != "" {
			return fmt.Errorf("no user processes were dumped (last: %s)", lastDumpErr)
		}
		return fmt.Errorf("no user processes were dumped")
	}

	// Write metadata.
	meta := criuCheckpointMetadata{
		Name:       name,
		PIDs:       dumpedPIDs,
		Timestamp:  time.Now(),
		PipeInodes: pipeInodesMap,
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
	// Read children from /proc/<pid>/task/<pid>/children file.
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

// findExternalPipeInodes returns pipe inodes in the target process that
// are also held by init (us, PID 1). These are pipes that cross the dump
// boundary — one end in init, one end in the target — and must be
// externalized so CRIU doesn't try to checkpoint half a pipe.
func findExternalPipeInodes(pid int) []string {
	// Collect pipe inodes from the target process.
	targetPipes := make(map[string]bool)
	fdDir := fmt.Sprintf("/proc/%d/fd", pid)
	entries, err := os.ReadDir(fdDir)
	if err != nil {
		return nil
	}
	for _, e := range entries {
		link, err := os.Readlink(filepath.Join(fdDir, e.Name()))
		if err != nil || !strings.HasPrefix(link, "pipe:[") {
			continue
		}
		inode := strings.TrimSuffix(strings.TrimPrefix(link, "pipe:["), "]")
		targetPipes[inode] = true
	}

	// Also check direct children (CRIU dumps the whole tree).
	if data, err := os.ReadFile(fmt.Sprintf("/proc/%d/task/%d/children", pid, pid)); err == nil {
		for _, f := range strings.Fields(string(data)) {
			childPid, err := strconv.Atoi(f)
			if err != nil {
				continue
			}
			childFdDir := fmt.Sprintf("/proc/%d/fd", childPid)
			childEntries, err := os.ReadDir(childFdDir)
			if err != nil {
				continue
			}
			for _, ce := range childEntries {
				link, err := os.Readlink(filepath.Join(childFdDir, ce.Name()))
				if err != nil || !strings.HasPrefix(link, "pipe:[") {
					continue
				}
				inode := strings.TrimSuffix(strings.TrimPrefix(link, "pipe:["), "]")
				targetPipes[inode] = true
			}
		}
	}

	if len(targetPipes) == 0 {
		return nil
	}

	// Collect pipe inodes from init (us).
	initPipes := make(map[string]bool)
	selfEntries, err := os.ReadDir("/proc/self/fd")
	if err != nil {
		return nil
	}
	for _, e := range selfEntries {
		link, err := os.Readlink(filepath.Join("/proc/self/fd", e.Name()))
		if err != nil || !strings.HasPrefix(link, "pipe:[") {
			continue
		}
		inode := strings.TrimSuffix(strings.TrimPrefix(link, "pipe:["), "]")
		initPipes[inode] = true
	}

	// External = pipe inodes present in both init and the target tree.
	var external []string
	for inode := range targetPipes {
		if initPipes[inode] {
			external = append(external, inode)
		}
	}
	sort.Strings(external)
	return external
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

		// For each external pipe inode, open /dev/null as a replacement fd.
		// CRIU will wire the restored process's pipe endpoints to /dev/null,
		// so reads return EOF (how fork children detect they're restored).
		var extraFiles []*os.File
		if inodes := meta.PipeInodes[pid]; len(inodes) > 0 {
			for i, inode := range inodes {
				f, err := os.Open("/dev/null")
				if err != nil {
					return fmt.Errorf("open /dev/null for pipe inherit: %w", err)
				}
				extraFiles = append(extraFiles, f)
				fdNum := 3 + i // ExtraFiles[i] → fd 3+i in CRIU's process
				args = append(args, "--inherit-fd", fmt.Sprintf("fd[%d]:pipe:[%s]", fdNum, inode))
			}
		}

		slog.Info("criu restore", "pid", pid, "dir", pidDir, "inherit_pipes", len(extraFiles))
		cmd := exec.Command("criu", args...)
		cmd.ExtraFiles = extraFiles
		output, err := cmd.CombinedOutput()
		for _, f := range extraFiles {
			f.Close()
		}
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
