//go:build linux

package main

import (
	"crypto/ed25519"
	"crypto/rand"
	"io"
	"log/slog"
	"os"
	"os/exec"
	"syscall"

	"github.com/creack/pty"
	"github.com/gliderlabs/ssh"
	"github.com/mdlayher/vsock"
	"github.com/semistrict/lnx/internal/protocol"
	gossh "golang.org/x/crypto/ssh"
	"golang.org/x/sys/unix"
)

// startSSHServer starts an embedded SSH server on a vsock port.
// It accepts any public key (vsock is host-only) and runs commands
// using the same exec setup as the normal exec server.
func startSSHServer() {
	ln, err := vsock.Listen(protocol.SSHPort, nil)
	if err != nil {
		slog.Warn("ssh server listen failed", "error", err)
		return
	}

	_, privKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		slog.Warn("ssh host key generation failed", "error", err)
		ln.Close()
		return
	}
	signer, err := gossh.NewSignerFromKey(privKey)
	if err != nil {
		slog.Warn("ssh signer creation failed", "error", err)
		ln.Close()
		return
	}

	forwardHandler := &ssh.ForwardedTCPHandler{}
	server := &ssh.Server{
		Handler: handleSSHSession,
		PublicKeyHandler: func(ctx ssh.Context, key ssh.PublicKey) bool {
			return true // vsock is host-only, no network exposure
		},
		LocalPortForwardingCallback: func(ctx ssh.Context, dhost string, dport uint32) bool {
			return true // allow all port forwarding (needed for VS Code Remote)
		},
		ReversePortForwardingCallback: func(ctx ssh.Context, bhost string, bport uint32) bool {
			return true
		},
		ChannelHandlers: map[string]ssh.ChannelHandler{
			"session":      ssh.DefaultSessionHandler,
			"direct-tcpip": ssh.DirectTCPIPHandler,
		},
		RequestHandlers: map[string]ssh.RequestHandler{
			"tcpip-forward":        forwardHandler.HandleSSHRequest,
			"cancel-tcpip-forward": forwardHandler.HandleSSHRequest,
		},
	}
	server.AddHostKey(signer)

	go func() {
		if err := server.Serve(ln); err != nil {
			slog.Debug("ssh server stopped", "error", err)
		}
	}()

	slog.Info("ssh server started", "port", protocol.SSHPort)
}

func handleSSHSession(s ssh.Session) {
	args := s.Command()
	if len(args) == 0 {
		args = []string{"bash", "-l"}
	}

	cmd := exec.Command(args[0], args[1:]...)
	cmd.Env = os.Environ()
	for _, kv := range s.Environ() {
		cmd.Env = append(cmd.Env, kv)
	}

	switch {
	case setupCWD != "":
		cmd.Dir = setupCWD
	default:
		cmd.Dir = os.Getenv("HOME")
	}

	cmd.SysProcAttr = &syscall.SysProcAttr{
		Setsid: true,
	}
	if setupUID > 0 {
		cmd.SysProcAttr.Credential = &syscall.Credential{
			Uid:    uint32(setupUID),
			Gid:    uint32(setupUID),
			Groups: lookupSupplementaryGroups(setupUID),
		}
	}

	ptyReq, winCh, isPTY := s.Pty()
	if isPTY {
		cmd.Env = append(cmd.Env, "TERM="+ptyReq.Term)

		ptmx, err := pty.Start(cmd)
		if err != nil {
			slog.Warn("ssh pty start failed", "args", args, "error", err)
			s.Exit(127)
			return
		}
		defer ptmx.Close()

		if ptyReq.Window.Height > 0 && ptyReq.Window.Width > 0 {
			unix.IoctlSetWinsize(int(ptmx.Fd()), unix.TIOCSWINSZ, &unix.Winsize{
				Row: uint16(ptyReq.Window.Height),
				Col: uint16(ptyReq.Window.Width),
			})
		}

		go func() {
			for win := range winCh {
				unix.IoctlSetWinsize(int(ptmx.Fd()), unix.TIOCSWINSZ, &unix.Winsize{
					Row: uint16(win.Height),
					Col: uint16(win.Width),
				})
			}
		}()

		go io.Copy(ptmx, s)
		io.Copy(s, ptmx)

		exitCode := 0
		if err := cmd.Wait(); err != nil {
			if exitErr, ok := err.(*exec.ExitError); ok {
				exitCode = exitErr.ExitCode()
			} else {
				exitCode = 127
			}
		}
		s.Exit(exitCode)
	} else {
		cmd.Stdin = s
		cmd.Stdout = s
		cmd.Stderr = s.Stderr()

		exitCode := 0
		if err := cmd.Run(); err != nil {
			if exitErr, ok := err.(*exec.ExitError); ok {
				exitCode = exitErr.ExitCode()
			} else {
				slog.Warn("ssh exec failed", "args", args, "error", err)
				exitCode = 127
			}
		}
		s.Exit(exitCode)
	}
}
