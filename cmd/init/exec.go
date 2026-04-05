//go:build linux

package main

import (
	"encoding/gob"
	"fmt"
	"io"
	"log/slog"
	"os"
	"os/exec"
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
	ptyFd *os.File
	mu    sync.Mutex
}

func (s *execSession) setProcess(p *os.Process) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.proc = p
}

func (s *execSession) setPTY(f *os.File) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.ptyFd = f
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
			s.mu.Lock()
			proc := s.proc
			s.mu.Unlock()
			if proc != nil {
				_ = proc.Signal(syscall.Signal(msg.ExecSignal.Sig))
			}
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

	sess := &execSession{}
	go sess.readControlMessages(dec)

	if msg.ExecReq.PTY {
		runExecPTY(enc, msg.ExecReq, interactiveLn, sess)
	} else {
		runExecPipe(enc, msg.ExecReq, sess)
	}
}

// runExecPTY handles an interactive exec request with a PTY.
func runExecPTY(enc *gob.Encoder, req *protocol.ExecReq, ln *vsock.Listener, sess *execSession) {
	if len(req.Args) == 0 {
		enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
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
	if setupUID > 0 {
		cmd.SysProcAttr = &syscall.SysProcAttr{
			Credential: &syscall.Credential{
				Uid:    uint32(setupUID),
				Gid:    uint32(setupUID),
				Groups: lookupSupplementaryGroups(setupUID),
			},
		}
	}

	ptmx, err := pty.Start(cmd)
	if err != nil {
		if len(req.Args) > 0 {
			commandNotFound(enc, req.Args, err)
		}
		enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
		return
	}
	defer ptmx.Close()

	// Report guest PID to host.
	enc.Encode(protocol.Msg{ExecStarted: &protocol.ExecStarted{PID: cmd.Process.Pid}})

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
		slog.Debug("exec interactive accept failed", "error", err)
		cmd.Process.Kill()
		cmd.Wait()
		enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
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
	cmd.Process.Signal(syscall.SIGHUP)

	exitCode := 0
	select {
	case err := <-waitCh:
		if exitErr, ok := err.(*exec.ExitError); ok {
			exitCode = exitErr.ExitCode()
		} else if err != nil {
			exitCode = 127
		}
	case <-time.After(3 * time.Second):
		cmd.Process.Kill()
		err := <-waitCh
		if exitErr, ok := err.(*exec.ExitError); ok {
			exitCode = exitErr.ExitCode()
		} else if err != nil {
			exitCode = 137
		}
	}

	enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: exitCode}})
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
func runExecPipe(enc *gob.Encoder, req *protocol.ExecReq, sess *execSession) {
	if len(req.Args) == 0 {
		enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
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
	if setupUID > 0 {
		cmd.SysProcAttr = &syscall.SysProcAttr{
			Credential: &syscall.Credential{
				Uid:    uint32(setupUID),
				Gid:    uint32(setupUID),
				Groups: lookupSupplementaryGroups(setupUID),
			},
		}
	}

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
		return
	}
	stderr, err := cmd.StderrPipe()
	if err != nil {
		enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
		return
	}

	if err := cmd.Start(); err != nil {
		enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
		return
	}

	// Report guest PID to host.
	enc.Encode(protocol.Msg{ExecStarted: &protocol.ExecStarted{PID: cmd.Process.Pid}})

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
				if encErr := enc.Encode(protocol.Msg{ExecOutput: out}); encErr != nil {
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

	enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: exitCode}})
}
