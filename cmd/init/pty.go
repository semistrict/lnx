//go:build linux

package main

import (
	"fmt"
	"os"
	"os/exec"
	"syscall"

	"github.com/creack/pty"
	"github.com/mdlayher/vsock"
	"github.com/semistrict/lnx/internal/protocol"
	"golang.org/x/sys/unix"
)

func runWithPTY(args []string, cwdPath string, uid int, rows, cols uint16, exitCode *int) error {
	termConn, err := vsock.Dial(vsockHostCID, protocol.TerminalPort, nil)
	if err != nil {
		return fmt.Errorf("vsock dial terminal: %w", err)
	}
	defer termConn.Close()

	cmd := exec.Command(args[0], args[1:]...)
	if cwdPath != "" {
		cmd.Dir = cwdPath
	}
	cmd.Env = os.Environ()
	if uid > 0 {
		cmd.SysProcAttr = &syscall.SysProcAttr{
			Credential: &syscall.Credential{
				Uid: uint32(uid),
				Gid: uint32(uid),
			},
		}
	}

	ptmx, err := pty.Start(cmd)
	if err != nil {
		return fmt.Errorf("pty start: %w", err)
	}
	defer ptmx.Close()

	// Set initial terminal size from the host.
	if rows > 0 && cols > 0 {
		_ = unix.IoctlSetWinsize(int(ptmx.Fd()), unix.TIOCSWINSZ, &unix.Winsize{
			Row: rows,
			Col: cols,
		})
	}

	// Register PTY so the control reader can resize it on SIGWINCH.
	setControlPTY(ptmx)
	defer setControlPTY(nil)

	setControlProcess(cmd.Process)
	defer setControlProcess(nil)

	// vsock → ptmx (stdin from host)
	go func() {
		buf := make([]byte, 4096)
		for {
			n, err := termConn.Read(buf)
			if n > 0 {
				ptmx.Write(buf[:n])
			}
			if err != nil {
				return
			}
		}
	}()

	// ptmx → vsock (stdout to host)
	buf := make([]byte, 4096)
	for {
		n, err := ptmx.Read(buf)
		if n > 0 {
			termConn.Write(buf[:n])
		}
		if err != nil {
			break
		}
	}

	if err := cmd.Wait(); err != nil {
		if exitErr, ok := err.(*exec.ExitError); ok {
			*exitCode = exitErr.ExitCode()
		} else {
			*exitCode = 127
		}
	}
	return nil
}
