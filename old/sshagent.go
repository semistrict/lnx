package lnx

import (
	"encoding/binary"
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

// countSSHKeys queries the SSH agent for the number of loaded identities.
// Uses the SSH agent protocol directly: sends SSH_AGENTC_REQUEST_IDENTITIES (11),
// reads SSH_AGENT_IDENTITIES_ANSWER (12) with the key count.
func countSSHKeys(authSock string) (int, error) {
	conn, err := net.Dial("unix", authSock)
	if err != nil {
		return 0, err
	}
	defer conn.Close()

	// Request identities: length(1) + type(11)
	req := []byte{0, 0, 0, 1, 11}
	if _, err := conn.Write(req); err != nil {
		return 0, err
	}

	// Read response: 4-byte length, 1-byte type, 4-byte count
	var respLen uint32
	if err := binary.Read(conn, binary.BigEndian, &respLen); err != nil {
		return 0, err
	}
	if respLen < 5 {
		return 0, nil
	}

	var msgType byte
	if err := binary.Read(conn, binary.BigEndian, &msgType); err != nil {
		return 0, err
	}
	if msgType != 12 { // SSH_AGENT_IDENTITIES_ANSWER
		return 0, nil
	}

	var count uint32
	if err := binary.Read(conn, binary.BigEndian, &count); err != nil {
		return 0, err
	}
	return int(count), nil
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
