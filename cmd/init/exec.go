//go:build linux

package main

import (
	"encoding/gob"
	"fmt"
	"io"
	"log/slog"
	"os"
	"os/exec"
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

// listUserPIDs scans /proc for all session-leader processes owned by the
// setup user. These are the top-level process trees that CRIU should dump.
// Excludes PID 1 (init) and kernel threads.
func listUserPIDs() []int {
	entries, err := os.ReadDir("/proc")
	if err != nil {
		return nil
	}

	myPID := os.Getpid()
	var pids []int

	for _, e := range entries {
		pid, err := strconv.Atoi(e.Name())
		if err != nil || pid <= 1 || pid == myPID {
			continue
		}

		// Read process status to check UID and session ID.
		statusPath := fmt.Sprintf("/proc/%d/status", pid)
		data, err := os.ReadFile(statusPath)
		if err != nil {
			continue // process may have exited
		}

		// Parse UID line: "Uid:\treal\teffective\tsaved\tfs"
		uid := -1
		sid := -1
		for _, line := range strings.Split(string(data), "\n") {
			if strings.HasPrefix(line, "Uid:") {
				fields := strings.Fields(line)
				if len(fields) >= 2 {
					uid, _ = strconv.Atoi(fields[1])
				}
			}
		}

		// Read session ID from /proc/<pid>/stat (field 6).
		statData, err := os.ReadFile(fmt.Sprintf("/proc/%d/stat", pid))
		if err != nil {
			continue
		}
		// Skip past comm field (may contain spaces/parens).
		if idx := strings.LastIndex(string(statData), ")"); idx >= 0 {
			fields := strings.Fields(string(statData)[idx+2:])
			if len(fields) >= 4 {
				sid, _ = strconv.Atoi(fields[3]) // field 6 = session ID (0-indexed field 3 after ")")
			}
		}

		// Only include user processes that are session leaders.
		// Session leader: PID == SID.
		if setupUID > 0 && uid != setupUID {
			continue
		}
		if sid != pid {
			continue // not a session leader
		}

		pids = append(pids, pid)
	}

	sort.Ints(pids)
	return pids
}

const (
	// forkRequestFD is the fd number the child writes to to request a fork.
	// Passed as ExtraFiles[0] → child sees it as fd 3.
	forkRequestFD = 3
	// forkResultFD is the fd number the child reads from to get the fork result.
	// Passed as ExtraFiles[1] → child sees it as fd 4.
	forkResultFD = 4
)

// ensureCloseOnExec sets close-on-exec on all file descriptors from minFD
// upward. This is a safety net against fd leaks (e.g. vsock fds without
// CLOEXEC) that would otherwise be inherited by child processes and cause
// CRIU to fail with "Unknown socket collected (family 40)".
// Go's exec.Cmd already closes extra fds in the child, but this catches
// any fds that slip through (race between goroutines creating fds and fork).
func ensureCloseOnExec(minFD int) {
	entries, err := os.ReadDir("/proc/self/fd")
	if err != nil {
		return
	}
	for _, e := range entries {
		fd, err := strconv.Atoi(e.Name())
		if err != nil || fd < minFD {
			continue
		}
		unix.CloseOnExec(fd)
	}
}

// createForkPipes creates the pipe pairs for fork communication.
// Returns (requestRead, requestWrite, resultRead, resultWrite).
// requestWrite (fd 3) and resultRead (fd 4) go to the child via ExtraFiles.
// requestRead and resultWrite are kept by init.
func createForkPipes() (reqR, reqW, resR, resW *os.File, err error) {
	rr, rw, err := os.Pipe()
	if err != nil {
		return nil, nil, nil, nil, fmt.Errorf("fork request pipe: %w", err)
	}
	sr, sw, err := os.Pipe()
	if err != nil {
		rr.Close()
		rw.Close()
		return nil, nil, nil, nil, fmt.Errorf("fork result pipe: %w", err)
	}
	return rr, rw, sr, sw, nil
}

// handleForkPipe reads fork requests from the pipe and triggers forks.
// Runs until the pipe is closed (child exited). After a successful fork,
// sends a ForkNotify on the exec session's gob connection so the host
// can tell the CLI about the new child instance.
func handleForkPipe(reqR, resW *os.File, sess *execSession) {
	defer reqR.Close()
	defer resW.Close()

	buf := make([]byte, 64)
	for {
		n, err := reqR.Read(buf)
		if err != nil {
			return // pipe closed, child exited
		}
		cmd := strings.TrimSpace(string(buf[:n]))
		if cmd != "fork" {
			continue
		}

		// Trigger fork via the guest control handler.
		result := doGuestFork()

		// Notify the host BEFORE writing to the result pipe — the parent
		// process may exit immediately after reading the result, and the
		// CLI needs to know about the fork before ExecDone arrives.
		if !strings.HasPrefix(result, "error:") {
			sess.encode(protocol.Msg{ForkNotify: &protocol.ForkNotify{Instance: result}})
		}

		resW.Write([]byte(result + "\n"))
	}
}

// doGuestFork triggers a CRIU fork dump and asks the host to clone.
// Returns the child instance name or "error: ...".
func doGuestFork() string {
	if err := criuDump("fork", criuForkDir, true); err != nil {
		return "error: " + err.Error()
	}
	syscall.Sync()

	// Ask the host to clone rootfs + CRIU volume and spawn child.
	gc := getGuestControl()
	if gc == nil {
		return "error: guest control not available"
	}

	gc.mu.Lock()
	defer gc.mu.Unlock()

	if err := gc.enc.Encode(protocol.Msg{ForkReq: &protocol.ForkReq{}}); err != nil {
		return "error: " + err.Error()
	}

	var msg protocol.Msg
	if err := gc.dec.Decode(&msg); err != nil {
		return "error: " + err.Error()
	}
	if msg.ForkResp == nil {
		return "error: unexpected response"
	}
	if msg.ForkResp.Error != "" {
		return "error: " + msg.ForkResp.Error
	}

	// Clean up fork dump from parent.
	os.RemoveAll(criuForkDir)
	return msg.ForkResp.Instance
}

// startExecServer listens on the exec vsock port and handles one
// exec request per connection. Multiple connections are accepted
// concurrently so `lnx exec` works while the main command runs.
func startExecServer() {
	execLn, err := vsock.Listen(protocol.ExecPort, nil)
	if err != nil {
		slog.Warn("exec listen failed", "error", err)
		return
	}

	interactiveLn, err := vsock.Listen(protocol.ExecInteractivePort, nil)
	if err != nil {
		slog.Warn("exec interactive listen failed", "error", err)
		execLn.Close()
		return
	}

	go func() {
		for {
			conn, err := execLn.Accept()
			if err != nil {
				return
			}
			go handleExecConn(conn.(*vsock.Conn), interactiveLn)
		}
	}()
}

// execSession holds per-session state for signal/resize forwarding.
type execSession struct {
	proc  *os.Process
	pgid  int
	ptyFd *os.File
	mu    sync.Mutex

	// encMu serializes writes to the gob encoder (used by main goroutine
	// for ExecStarted/ExecDone and by handleForkPipe for ForkNotify).
	encMu sync.Mutex
	enc   *gob.Encoder
}

// encode sends a gob message on the session's exec connection, safely.
func (s *execSession) encode(msg protocol.Msg) error {
	s.encMu.Lock()
	defer s.encMu.Unlock()
	return s.enc.Encode(msg)
}

func (s *execSession) setProcess(p *os.Process) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.proc = p
	s.pgid = 0
	if p != nil {
		if pgid, err := syscall.Getpgid(p.Pid); err == nil {
			s.pgid = pgid
		}
	}
}

func (s *execSession) setPTY(f *os.File) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.ptyFd = f
}

func (s *execSession) signal(sig syscall.Signal) error {
	s.mu.Lock()
	proc := s.proc
	pgid := s.pgid
	s.mu.Unlock()

	if pgid > 0 {
		return syscall.Kill(-pgid, sig)
	}
	if proc != nil {
		return proc.Signal(sig)
	}
	return nil
}

// readControlMessages reads ExecSignal and ExecResize messages from the gob
// decoder and applies them to this session's process/PTY. Runs until the
// connection closes or an error occurs.
func (s *execSession) readControlMessages(dec *gob.Decoder) {
	for {
		var msg protocol.Msg
		if err := dec.Decode(&msg); err != nil {
			return
		}
		if msg.ExecSignal != nil {
			_ = s.signal(syscall.Signal(msg.ExecSignal.Sig))
		}
		if msg.ExecResize != nil {
			s.mu.Lock()
			f := s.ptyFd
			s.mu.Unlock()
			if f != nil {
				_ = unix.IoctlSetWinsize(int(f.Fd()), unix.TIOCSWINSZ, &unix.Winsize{
					Row: msg.ExecResize.Rows,
					Col: msg.ExecResize.Cols,
				})
			}
		}
	}
}

func handleExecConn(conn *vsock.Conn, interactiveLn *vsock.Listener) {
	defer conn.Close()
	enc := gob.NewEncoder(conn)
	dec := gob.NewDecoder(conn)

	var msg protocol.Msg
	if err := dec.Decode(&msg); err != nil {
		return
	}
	if msg.ExecReq == nil {
		return
	}

	sess := &execSession{enc: enc}
	go sess.readControlMessages(dec)

	if msg.ExecReq.PTY {
		runExecPTY(msg.ExecReq, interactiveLn, sess)
	} else {
		runExecPipe(msg.ExecReq, sess)
	}
}

// runExecPTY handles an interactive exec request with a PTY.
func runExecPTY(req *protocol.ExecReq, ln *vsock.Listener, sess *execSession) {
	if len(req.Args) == 0 {
		sess.encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
		return
	}

	cmd := exec.Command(req.Args[0], req.Args[1:]...)
	cmd.Env = os.Environ()
	for _, kv := range req.Env {
		cmd.Env = append(cmd.Env, kv)
	}
	switch {
	case req.CWD != "":
		cmd.Dir = req.CWD
	case setupCWD != "":
		cmd.Dir = setupCWD
	default:
		cmd.Dir = os.Getenv("HOME")
	}
	cmd.SysProcAttr = &syscall.SysProcAttr{
		Setsid: true, // CRIU requires session leaders
	}
	if setupUID > 0 {
		cmd.SysProcAttr.Credential = &syscall.Credential{
			Uid:    uint32(setupUID),
			Gid:    uint32(setupUID),
			Groups: lookupSupplementaryGroups(setupUID),
		}
	}

	// Fork pipes: child gets fd 3 (write fork request) and fd 4 (read result).
	reqR, reqW, resR, resW, err := createForkPipes()
	if err != nil {
		slog.Warn("create fork pipes failed", "error", err)
		sess.encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
		return
	}
	cmd.ExtraFiles = []*os.File{reqW, resR} // fd 3, fd 4 in child
	defer reqR.Close()
	defer resW.Close()

	ensureCloseOnExec(3) // prevent vsock/other fd leaks to child

	ptmx, err := pty.Start(cmd)
	if err != nil {
		reqW.Close()
		resR.Close()
		slog.Warn("exec pty start failed", "args", req.Args, "error", err)
		if len(req.Args) > 0 {
			commandNotFound(sess.enc, req.Args, err)
		}
		sess.encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
		return
	}
	defer ptmx.Close()

	// Close child's end of fork pipes (they're dup'd into the child).
	reqW.Close()
	resR.Close()

	// Handle fork requests from the child in the background.
	go handleForkPipe(reqR, resW, sess)

	// Report guest PID to host.
	sess.encode(protocol.Msg{ExecStarted: &protocol.ExecStarted{PID: cmd.Process.Pid}})

	if req.Rows > 0 && req.Cols > 0 {
		unix.IoctlSetWinsize(int(ptmx.Fd()), unix.TIOCSWINSZ, &unix.Winsize{
			Row: req.Rows,
			Col: req.Cols,
		})
	}

	// Register PTY and process with the per-session handler for signals/resize.
	sess.setPTY(ptmx)
	sess.setProcess(cmd.Process)

	// Accept connection from host for raw terminal I/O.
	vsockConn, err := ln.Accept()
	if err != nil {
		slog.Warn("exec interactive accept failed", "args", req.Args, "error", err)
		cmd.Process.Kill()
		cmd.Wait()
		sess.encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
		return
	}
	defer vsockConn.Close()

	// Splice: vsock ↔ PTY.
	done := make(chan struct{})
	go func() {
		io.Copy(ptmx, vsockConn)
		close(done)
	}()
	io.Copy(vsockConn, ptmx)
	vsockConn.Close()
	<-done

	// Connection dropped. If the process is still running, give it a chance
	// to exit gracefully (SIGHUP, like a terminal hangup), then force-kill.
	waitCh := make(chan error, 1)
	go func() { waitCh <- cmd.Wait() }()

	// Try SIGHUP first (terminal hangup — shells handle this).
	_ = sess.signal(syscall.SIGHUP)

	exitCode := 0
	select {
	case err := <-waitCh:
		if exitErr, ok := err.(*exec.ExitError); ok {
			exitCode = exitErr.ExitCode()
		} else if err != nil {
			exitCode = 127
		}
	case <-time.After(3 * time.Second):
		_ = sess.signal(syscall.SIGKILL)
		err := <-waitCh
		if exitErr, ok := err.(*exec.ExitError); ok {
			exitCode = exitErr.ExitCode()
		} else if err != nil {
			exitCode = 137
		}
	}

	sess.encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: exitCode}})
}

// commandNotFound writes "name: command not found" to the gob encoder
// as ExecOutput when the error is ErrNotFound.
func commandNotFound(enc *gob.Encoder, args []string, err error) {
	// pty.Start wraps the error, check the message
	if cmd_err, ok := err.(*exec.Error); ok && cmd_err.Err == exec.ErrNotFound {
		enc.Encode(protocol.Msg{ExecOutput: &protocol.ExecOutput{
			Stderr: []byte(args[0] + ": command not found\n"),
		}})
	}
}

// lookupSupplementaryGroups reads /etc/group to find all groups that contain
// the user with the given UID. Returns the group IDs for use in Credential.Groups.
func lookupSupplementaryGroups(uid int) []uint32 {
	// Find the username for this UID from /etc/passwd.
	var username string
	if data, err := os.ReadFile("/etc/passwd"); err == nil {
		for _, line := range strings.Split(string(data), "\n") {
			parts := strings.SplitN(line, ":", 4)
			if len(parts) >= 3 {
				if uidStr := parts[2]; uidStr == fmt.Sprintf("%d", uid) {
					username = parts[0]
					break
				}
			}
		}
	}
	if username == "" {
		return nil
	}

	var groups []uint32
	if data, err := os.ReadFile("/etc/group"); err == nil {
		for _, line := range strings.Split(string(data), "\n") {
			parts := strings.SplitN(line, ":", 4)
			if len(parts) != 4 {
				continue
			}
			for _, member := range strings.Split(parts[3], ",") {
				if strings.TrimSpace(member) == username {
					if gid, err := strconv.Atoi(parts[2]); err == nil {
						groups = append(groups, uint32(gid))
					}
					break
				}
			}
		}
	}
	return groups
}

// runExecPipe handles a non-interactive exec request with piped stdout/stderr.
func runExecPipe(req *protocol.ExecReq, sess *execSession) {
	if len(req.Args) == 0 {
		sess.encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
		return
	}

	cmd := exec.Command(req.Args[0], req.Args[1:]...)
	cmd.Env = os.Environ()
	for _, kv := range req.Env {
		cmd.Env = append(cmd.Env, kv)
	}
	switch {
	case req.CWD != "":
		cmd.Dir = req.CWD
	case setupCWD != "":
		cmd.Dir = setupCWD
	default:
		cmd.Dir = os.Getenv("HOME")
	}
	cmd.SysProcAttr = &syscall.SysProcAttr{
		Setsid: true, // CRIU requires session leaders
	}
	if setupUID > 0 {
		cmd.SysProcAttr.Credential = &syscall.Credential{
			Uid:    uint32(setupUID),
			Gid:    uint32(setupUID),
			Groups: lookupSupplementaryGroups(setupUID),
		}
	}

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		sess.encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
		return
	}
	stderr, err := cmd.StderrPipe()
	if err != nil {
		sess.encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
		return
	}

	// Fork pipes: child gets fd 3 (write fork request) and fd 4 (read result).
	reqR, reqW, resR, resW, err := createForkPipes()
	if err != nil {
		slog.Warn("create fork pipes failed", "error", err)
		sess.encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
		return
	}
	cmd.ExtraFiles = []*os.File{reqW, resR} // fd 3, fd 4 in child

	ensureCloseOnExec(3) // prevent vsock/other fd leaks to child

	if err := cmd.Start(); err != nil {
		reqR.Close()
		reqW.Close()
		resR.Close()
		resW.Close()
		commandNotFound(sess.enc, req.Args, err)
		sess.encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
		return
	}

	// Close child's end of fork pipes.
	reqW.Close()
	resR.Close()

	// Handle fork requests from the child in the background.
	go handleForkPipe(reqR, resW, sess)

	// Report guest PID to host.
	sess.encode(protocol.Msg{ExecStarted: &protocol.ExecStarted{PID: cmd.Process.Pid}})

	sess.setProcess(cmd.Process)

	done := make(chan struct{}, 2)
	stream := func(r io.Reader, isStderr bool) {
		defer func() { done <- struct{}{} }()
		buf := make([]byte, 4096)
		for {
			n, err := r.Read(buf)
			if n > 0 {
				out := &protocol.ExecOutput{}
				data := make([]byte, n)
				copy(data, buf[:n])
				if isStderr {
					out.Stderr = data
				} else {
					out.Stdout = data
				}
				if encErr := sess.encode(protocol.Msg{ExecOutput: out}); encErr != nil {
					return
				}
			}
			if err != nil {
				return
			}
		}
	}

	go stream(stdout, false)
	go stream(stderr, true)
	<-done
	<-done

	exitCode := 0
	if err := cmd.Wait(); err != nil {
		if exitErr, ok := err.(*exec.ExitError); ok {
			exitCode = exitErr.ExitCode()
		} else {
			exitCode = 127
		}
	}

	sess.encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: exitCode}})
}
