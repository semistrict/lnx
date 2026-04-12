//go:build linux

package main

import (
	"encoding/gob"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/creack/pty"
	"github.com/mdlayher/vsock"
	"github.com/semistrict/lnx/internal/protocol"
	"golang.org/x/sys/unix"
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

// forkSession holds the PTY master and criu command of a CRIU-restored fork
// child, so the fork attach server can serve it to the host.
type forkSession struct {
	ptmx       *os.File   // PTY master — restored process has the slave
	pts        *os.File   // PTY slave — kept open to prevent EIO until host connects
	pid        int        // restored process PID (session leader)
	cmd        *exec.Cmd  // criu restore command (nil if --restore-detached was used)
	cleanupDir string     // directory to remove after criu exits
	extraPTMX  *os.File   // criu's session PTY master (tty path); kept alive to prevent SIGHUP
}

var pendingFork struct {
	mu   sync.Mutex
	sess *forkSession
}

func setPendingForkSession(fs *forkSession) {
	pendingFork.mu.Lock()
	pendingFork.sess = fs
	pendingFork.mu.Unlock()
}

func consumePendingForkSession() *forkSession {
	pendingFork.mu.Lock()
	defer pendingFork.mu.Unlock()
	fs := pendingFork.sess
	pendingFork.sess = nil
	return fs
}

// criuCheckpointMetadata is written alongside CRIU image dirs so we
// know what was dumped.
type criuCheckpointMetadata struct {
	Name       string           `json:"name"`
	PIDs       []int            `json:"pids"`
	Timestamp  time.Time        `json:"timestamp"`
	PipeInodes map[int][]string `json:"pipe_inodes,omitempty"` // PID → external pipe inodes
	StdioPipes map[int][]string `json:"stdio_pipes,omitempty"` // PID → [stdout_inode, stderr_inode]
	StdioTTY   map[int]uint64   `json:"stdio_tty,omitempty"`   // PID → tty rdev (if stdout is a PTY)
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
	stdioPipesMap := make(map[int][]string)
	stdioTTYMap := make(map[int]uint64)
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
		// Record stdout/stderr info so the restore path can wire them
		// to a PTY instead of /dev/null or a disconnected tty.
		stdioPipes := findStdioPipeInodes(pid)
		if len(stdioPipes) > 0 {
			stdioPipesMap[pid] = stdioPipes
		}
		if rdev := findStdioTTYRdev(pid); rdev != 0 {
			stdioTTYMap[pid] = rdev
		}
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
		StdioPipes: stdioPipesMap,
		StdioTTY:   stdioTTYMap,
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

// findStdioPipeInodes returns the pipe inodes for the process's stdout and
// stderr (fd 1 and fd 2), if they are pipes. Returns up to 2 inodes.
func findStdioPipeInodes(pid int) []string {
	var inodes []string
	for _, fd := range []string{"1", "2"} {
		link, err := os.Readlink(fmt.Sprintf("/proc/%d/fd/%s", pid, fd))
		if err != nil || !strings.HasPrefix(link, "pipe:[") {
			continue
		}
		inode := strings.TrimSuffix(strings.TrimPrefix(link, "pipe:["), "]")
		inodes = append(inodes, inode)
	}
	return inodes
}

// findStdioTTYRdev returns the rdev of stdout (fd 1) if it's a tty device.
// Returns 0 if stdout is not a tty.
func findStdioTTYRdev(pid int) uint64 {
	var st syscall.Stat_t
	path := fmt.Sprintf("/proc/%d/fd/1", pid)
	if err := syscall.Stat(path, &st); err != nil {
		return 0
	}
	// Check if it's a character device (tty).
	if st.Mode&syscall.S_IFMT != syscall.S_IFCHR {
		return 0
	}
	return st.Rdev
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

// criuRestoreForFork restores processes from CRIU images into a PTY so
// the host can attach and read the restored process's terminal output.
// The PTY master and first restored PID are stored as a pending fork session.
func criuRestoreForFork(dir string) error {
	metaPath := filepath.Join(dir, "metadata.json")
	data, err := os.ReadFile(metaPath)
	if err != nil {
		return fmt.Errorf("read metadata: %w", err)
	}

	var meta criuCheckpointMetadata
	if err := json.Unmarshal(data, &meta); err != nil {
		return fmt.Errorf("parse metadata: %w", err)
	}
	if len(meta.PIDs) == 0 {
		return fmt.Errorf("no PIDs in metadata")
	}

	// Create a PTY pair. Stdio pipes (stdout/stderr) from the dump are
	// wired to the PTY slave so output appears on our PTY master.
	// Other external pipes (fork pipes) are wired to /dev/null as before.
	ptmx, pts, err := pty.Open()
	if err != nil {
		return fmt.Errorf("open pty: %w", err)
	}

	var firstPID int
	var firstCriuPtmx *os.File
	for _, pid := range meta.PIDs {
		pidDir := filepath.Join(dir, strconv.Itoa(pid))
		if _, err := os.Stat(pidDir); err != nil {
			slog.Warn("criu fork restore: PID dir missing, skipping", "pid", pid, "dir", pidDir)
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

		// Build a set of stdio pipe inodes so we can wire them to the PTY.
		stdioSet := make(map[string]bool)
		for _, inode := range meta.StdioPipes[pid] {
			stdioSet[inode] = true
		}

		// Wire external pipe inodes: stdio pipes → PTY slave, others → /dev/null.
		var extraFiles []*os.File
		if inodes := meta.PipeInodes[pid]; len(inodes) > 0 {
			for _, inode := range inodes {
				var f *os.File
				if stdioSet[inode] {
					// Dup the PTY slave for each stdio pipe.
					dupFd, err := syscall.Dup(int(pts.Fd()))
					if err != nil {
						pts.Close()
						ptmx.Close()
						return fmt.Errorf("dup pty slave: %w", err)
					}
					f = os.NewFile(uintptr(dupFd), "pts-dup")
				} else {
					var err error
					f, err = os.Open("/dev/null")
					if err != nil {
						pts.Close()
						ptmx.Close()
						return fmt.Errorf("open /dev/null for pipe inherit: %w", err)
					}
				}
				fdNum := 3 + len(extraFiles)
				extraFiles = append(extraFiles, f)
				args = append(args, "--inherit-fd", fmt.Sprintf("fd[%d]:pipe:[%s]", fdNum, inode))
			}
		}

		// If stdout was a tty, map the tty device to our PTY slave.
		if rdev := meta.StdioTTY[pid]; rdev != 0 {
			dupFd, err := syscall.Dup(int(pts.Fd()))
			if err != nil {
				pts.Close()
				ptmx.Close()
				return fmt.Errorf("dup pty slave for tty: %w", err)
			}
			fdNum := 3 + len(extraFiles)
			extraFiles = append(extraFiles, os.NewFile(uintptr(dupFd), "pts-tty"))
			args = append(args, "--inherit-fd", fmt.Sprintf("fd[%d]:tty[%x]", fdNum, rdev))
		}

		slog.Info("criu fork restore", "pid", pid, "dir", pidDir,
			"pipes", len(extraFiles), "stdioPipes", len(stdioSet),
			"ttyRdev", fmt.Sprintf("0x%x", meta.StdioTTY[pid]))

		cmd := exec.Command("criu", args...)
		cmd.ExtraFiles = extraFiles
		if meta.StdioTTY[pid] != 0 {
			// For tty-based processes, criu needs a controlling terminal
			// session for --shell-job. Set stdin to our PTY slave.
			cmd.Stdin = pts
		}
		output, err := cmd.CombinedOutput()
		if err != nil {
			pts.Close()
			ptmx.Close()
			return fmt.Errorf("criu fork restore pid %d: %w\n%s", pid, err, string(output))
		}
		if firstPID == 0 {
			firstPID = pid
		}
	}

	if firstPID == 0 {
		pts.Close()
		ptmx.Close()
		return fmt.Errorf("no PIDs were restored")
	}

	// Keep pts open — if the restored process exits before the host
	// connects, the PTY slave reference keeps the master readable
	// (buffered data won't be lost to EIO).
	setPendingForkSession(&forkSession{ptmx: ptmx, pts: pts, pid: firstPID, cleanupDir: dir, extraPTMX: firstCriuPtmx})
	slog.Info("fork session ready", "pid", firstPID)
	return nil
}

// startForkAttachServer listens on the fork attach vsock ports and serves
// the pending fork session's PTY to a single host connection. After the
// restored process exits, the server shuts down.
func startForkAttachServer(fs *forkSession) {
	gobLn, err := vsock.Listen(protocol.ForkAttachPort, nil)
	if err != nil {
		slog.Error("fork attach listen failed", "port", protocol.ForkAttachPort, "error", err)
		fs.ptmx.Close()
		return
	}

	dataLn, err := vsock.Listen(protocol.ForkAttachDataPort, nil)
	if err != nil {
		slog.Error("fork attach data listen failed", "port", protocol.ForkAttachDataPort, "error", err)
		gobLn.Close()
		fs.ptmx.Close()
		return
	}

	go func() {
		defer gobLn.Close()
		defer dataLn.Close()
		defer fs.ptmx.Close()

		// Accept one gob connection from the host.
		gobConn, err := gobLn.Accept()
		if err != nil {
			slog.Error("fork attach accept failed", "error", err)
			return
		}
		defer gobConn.Close()
		enc := gob.NewEncoder(gobConn)
		dec := gob.NewDecoder(gobConn)

		// Read ExecReq for PTY dimensions.
		var msg protocol.Msg
		if err := dec.Decode(&msg); err != nil {
			slog.Error("fork attach read request failed", "error", err)
			return
		}
		if msg.ExecReq != nil && msg.ExecReq.Rows > 0 && msg.ExecReq.Cols > 0 {
			unix.IoctlSetWinsize(int(fs.ptmx.Fd()), unix.TIOCSWINSZ, &unix.Winsize{
				Row: msg.ExecReq.Rows,
				Col: msg.ExecReq.Cols,
			})
		}

		// Send ExecStarted.
		if err := enc.Encode(protocol.Msg{ExecStarted: &protocol.ExecStarted{PID: fs.pid}}); err != nil {
			slog.Error("fork attach send started failed", "error", err)
			return
		}

		// Accept PTY data connection from host.
		dataConn, err := dataLn.Accept()
		if err != nil {
			slog.Error("fork attach data accept failed", "error", err)
			return
		}
		defer dataConn.Close()

		// Now that the host is connected, close our extra PTY slave ref.
		// The restored process (if still alive) holds its own ref. When it
		// exits, the slave closes fully → master drains buffer then returns EIO.
		if fs.pts != nil {
			fs.pts.Close()
			fs.pts = nil
		}

		// Handle signals and resize from host.
		go func() {
			for {
				var msg protocol.Msg
				if err := dec.Decode(&msg); err != nil {
					return
				}
				if msg.ExecSignal != nil {
					syscall.Kill(-fs.pid, syscall.Signal(msg.ExecSignal.Sig))
				}
				if msg.ExecResize != nil {
					unix.IoctlSetWinsize(int(fs.ptmx.Fd()), unix.TIOCSWINSZ, &unix.Winsize{
						Row: msg.ExecResize.Rows,
						Col: msg.ExecResize.Cols,
					})
				}
			}
		}()

		// Splice PTY ↔ data connection.
		done := make(chan struct{})
		go func() {
			io.Copy(fs.ptmx, dataConn)
			close(done)
		}()
		io.Copy(dataConn, fs.ptmx)
		dataConn.Close()
		<-done

		// PTY read returned (slave closed — process exited). Collect exit code.
		exitCode := 0
		if fs.cmd != nil {
			if err := fs.cmd.Wait(); err != nil {
				if exitErr, ok := err.(*exec.ExitError); ok {
					exitCode = exitErr.ExitCode()
				} else {
					slog.Warn("fork attach cmd.Wait failed", "error", err)
					exitCode = 1
				}
			}
		} else {
			var ws syscall.WaitStatus
			_, err = syscall.Wait4(fs.pid, &ws, 0, nil)
			if err != nil {
				slog.Warn("fork attach wait4 failed", "pid", fs.pid, "error", err)
				exitCode = 1
			} else if ws.Exited() {
				exitCode = ws.ExitStatus()
			} else if ws.Signaled() {
				exitCode = 128 + int(ws.Signal())
			}
		}

		enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: exitCode}})

		// Clean up resources now that the process has exited.
		if fs.pts != nil {
			fs.pts.Close()
		}
		if fs.extraPTMX != nil {
			fs.extraPTMX.Close()
		}
		if fs.cleanupDir != "" {
			os.RemoveAll(fs.cleanupDir)
		}
	}()
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

		if err := criuRestoreForFork(criuForkDir); err != nil {
			slog.Error("CRIU fork restore failed", "error", err)
		} else {
			slog.Info("CRIU fork restore complete")
			// Start the fork attach server immediately so the PTY
			// buffer is read before the restored process exits.
			if fs := consumePendingForkSession(); fs != nil {
				startForkAttachServer(fs)
			}
		}
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
