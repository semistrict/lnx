//go:build linux

package main

import (
	"io"
	"log/slog"
	"net"
	"os"

	"github.com/mdlayher/vsock"
	"github.com/semistrict/lnx/internal/protocol"
)

const sshAgentSockPath = "/tmp/ssh-agent.sock"

// startSSHAgentForward creates a unix socket and proxies connections
// to the host's SSH agent via vsock.
func startSSHAgentForward() {
	os.Remove(sshAgentSockPath)

	listener, err := net.Listen("unix", sshAgentSockPath)
	if err != nil {
		slog.Warn("ssh agent listen failed", "error", err)
		return
	}
	// Make it accessible to the unprivileged user.
	os.Chmod(sshAgentSockPath, 0666)

	os.Setenv("SSH_AUTH_SOCK", sshAgentSockPath)

	go func() {
		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}
			go proxyToHostAgent(conn)
		}
	}()

	slog.Info("ssh agent forwarding enabled", "socket", sshAgentSockPath)
}

func proxyToHostAgent(clientConn net.Conn) {
	defer clientConn.Close()

	hostConn, err := vsock.Dial(vsockHostCID, protocol.SSHAgentPort, nil)
	if err != nil {
		slog.Debug("ssh agent vsock dial failed", "error", err)
		return
	}
	defer hostConn.Close()

	go io.Copy(hostConn, clientConn)
	io.Copy(clientConn, hostConn)
}
