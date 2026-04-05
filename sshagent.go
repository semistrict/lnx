package lnx

import (
	"io"
	"log/slog"
	"net"
)

// startSSHAgentProxy accepts connections from the guest on the given listener
// and proxies them to the host's SSH_AUTH_SOCK.
func startSSHAgentProxy(listener net.Listener, authSock string) {
	go func() {
		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}
			go proxySSHAgent(conn, authSock)
		}
	}()
}

func proxySSHAgent(guestConn net.Conn, authSock string) {
	defer guestConn.Close()

	hostConn, err := net.Dial("unix", authSock)
	if err != nil {
		slog.Debug("ssh agent dial failed", "error", err)
		return
	}
	defer hostConn.Close()

	go io.Copy(hostConn, guestConn)
	io.Copy(guestConn, hostConn)
}
