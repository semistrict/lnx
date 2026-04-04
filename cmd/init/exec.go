//go:build linux

package main

import (
	"encoding/gob"
	"io"
	"log/slog"
	"os"
	"os/exec"
	"syscall"

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

	if msg.ExecReq.PTY {
		runExecPTY(enc, msg.ExecReq, interactiveLn)
	} else {
		runExecPipe(enc, msg.ExecReq)
	}
}

// runExecPTY handles an interactive exec request with a PTY.
func runExecPTY(enc *gob.Encoder, req *protocol.ExecReq, ln *vsock.Listener) {
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
				Uid: uint32(setupUID),
				Gid: uint32(setupUID),
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

	if req.Rows > 0 && req.Cols > 0 {
		unix.IoctlSetWinsize(int(ptmx.Fd()), unix.TIOCSWINSZ, &unix.Winsize{
			Row: req.Rows,
			Col: req.Cols,
		})
	}

	// Register PTY for resize and process for signals from control connection.
	setControlPTY(ptmx)
	defer setControlPTY(nil)
	setControlProcess(cmd.Process)
	defer setControlProcess(nil)

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

// runExecPipe handles a non-interactive exec request with piped stdout/stderr.
func runExecPipe(enc *gob.Encoder, req *protocol.ExecReq) {
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
				Uid: uint32(setupUID),
				Gid: uint32(setupUID),
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
