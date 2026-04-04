//go:build linux

package main

import (
	"encoding/gob"
	"io"
	"log/slog"
	"os"
	"os/exec"

	"github.com/mdlayher/vsock"
	"github.com/semistrict/lnx/internal/protocol"
)

// startExecServer connects to the host on the exec vsock port
// and handles ExecReq messages for the VM's lifetime.
func startExecServer() {
	conn, err := vsock.Dial(vsockHostCID, protocol.ExecPort, nil)
	if err != nil {
		slog.Warn("exec vsock dial failed", "error", err)
		return
	}

	go func() {
		defer conn.Close()
		enc := gob.NewEncoder(conn)
		dec := gob.NewDecoder(conn)

		for {
			var msg protocol.Msg
			if err := dec.Decode(&msg); err != nil {
				return
			}
			if msg.ExecReq == nil {
				continue
			}
			runExec(enc, msg.ExecReq)
		}
	}()
}

func runExec(enc *gob.Encoder, req *protocol.ExecReq) {
	if len(req.Args) == 0 {
		enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
		return
	}

	cmd := exec.Command(req.Args[0], req.Args[1:]...)
	cmd.Env = os.Environ()
	for _, kv := range req.Env {
		cmd.Env = append(cmd.Env, kv)
	}

	// Run as the same user as the main process (set up in init).
	// Use the home directory as CWD.
	if home := os.Getenv("HOME"); home != "" {
		cmd.Dir = home
	}

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		slog.Debug("exec stdout pipe failed", "error", err)
		enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
		return
	}
	stderr, err := cmd.StderrPipe()
	if err != nil {
		slog.Debug("exec stderr pipe failed", "error", err)
		enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
		return
	}

	if err := cmd.Start(); err != nil {
		slog.Debug("exec start failed", "error", err)
		enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
		return
	}

	// Stream stdout and stderr in goroutines.
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

	// Wait for both streams to finish.
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
