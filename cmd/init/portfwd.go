//go:build linux

package main

import (
	"encoding/binary"
	"encoding/gob"
	"io"
	"log/slog"
	"net"
	"os"
	"strings"
	"time"

	"github.com/mdlayher/vsock"
	"github.com/semistrict/lnx/internal/protocol"
)

// startPortForwarder scans for listening TCP ports and notifies the host.
// It also listens on a vsock port for incoming forwarded connections from the host.
func startPortForwarder() {
	go runPortForwarder()
}

func runPortForwarder() {
	for {
		// Control connection: notify host of port changes.
		ctrlConn, err := vsock.Dial(vsockHostCID, protocol.PortForwardPort, nil)
		if err != nil {
			slog.Warn("port forward vsock dial failed", "error", err)
			time.Sleep(time.Second)
			continue
		}

		// Data listener: host connects here to forward TCP connections.
		dataLn, err := vsock.Listen(protocol.PortForwardDataPort, nil)
		if err != nil {
			slog.Warn("port forward data listen failed", "error", err)
			ctrlConn.Close()
			time.Sleep(time.Second)
			continue
		}

		// Accept forwarded connections from host.
		done := make(chan struct{})
		go acceptForwardedConns(dataLn, done)

		err = scanPorts(ctrlConn)
		ctrlConn.Close()
		_ = dataLn.Close()
		<-done
		if err != nil {
			slog.Warn("port forward control loop ended", "error", err)
		}
		time.Sleep(time.Second)
	}
}

func scanPorts(conn net.Conn) error {
	enc := gob.NewEncoder(conn)
	var prev []uint16

	for {
		ports := getListeningPorts()
		if !portsEqual(prev, ports) {
			if err := enc.Encode(protocol.PortForward{Ports: ports}); err != nil {
				return err
			}
			prev = ports
		}
		time.Sleep(2 * time.Second)
	}
}

// acceptForwardedConns accepts vsock connections from the host.
// Each connection starts with a 2-byte big-endian port number,
// then raw TCP data is spliced to localhost:port.
func acceptForwardedConns(ln *vsock.Listener, done chan<- struct{}) {
	defer close(done)
	for {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		go handleForwardedConn(conn)
	}
}

func handleForwardedConn(vsockConn net.Conn) {
	defer vsockConn.Close()

	// Read 2-byte target port.
	var portBuf [2]byte
	if _, err := io.ReadFull(vsockConn, portBuf[:]); err != nil {
		return
	}
	port := binary.BigEndian.Uint16(portBuf[:])

	// Connect to local service.
	local, err := net.Dial("tcp", net.JoinHostPort("127.0.0.1", itoa(int(port))))
	if err != nil {
		return
	}
	defer local.Close()

	// Splice. Close vsock when local→vsock finishes to propagate EOF.
	done := make(chan struct{})
	go func() {
		io.Copy(local, vsockConn)
		close(done)
	}()
	io.Copy(vsockConn, local)
	vsockConn.Close()
	<-done
}

// getListeningPorts reads /proc/net/tcp and /proc/net/tcp6 for LISTEN sockets.
func getListeningPorts() []uint16 {
	seen := map[uint16]bool{}
	for _, path := range []string{"/proc/net/tcp", "/proc/net/tcp6"} {
		data, err := os.ReadFile(path)
		if err != nil {
			continue
		}
		for _, line := range strings.Split(string(data), "\n")[1:] {
			fields := strings.Fields(line)
			if len(fields) < 4 {
				continue
			}
			// Field 3 is state: 0A = LISTEN
			if fields[3] != "0A" {
				continue
			}
			// Field 1 is local_address: ADDR:PORT (hex)
			parts := strings.SplitN(fields[1], ":", 2)
			if len(parts) != 2 {
				continue
			}
			port := hexToUint16(parts[1])
			if port > 0 && !guestInternalPort(port) {
				seen[port] = true
			}
		}
	}
	ports := make([]uint16, 0, len(seen))
	for p := range seen {
		ports = append(ports, p)
	}
	return ports
}

func hexToUint16(s string) uint16 {
	var n uint16
	for _, c := range s {
		n <<= 4
		switch {
		case c >= '0' && c <= '9':
			n |= uint16(c - '0')
		case c >= 'a' && c <= 'f':
			n |= uint16(c-'a') + 10
		case c >= 'A' && c <= 'F':
			n |= uint16(c-'A') + 10
		}
	}
	return n
}

func itoa(n int) string {
	if n == 0 {
		return "0"
	}
	var buf [5]byte
	i := len(buf)
	for n > 0 {
		i--
		buf[i] = byte('0' + n%10)
		n /= 10
	}
	return string(buf[i:])
}

func portsEqual(a, b []uint16) bool {
	if len(a) != len(b) {
		return false
	}
	am := map[uint16]bool{}
	for _, p := range a {
		am[p] = true
	}
	for _, p := range b {
		if !am[p] {
			return false
		}
	}
	return true
}
