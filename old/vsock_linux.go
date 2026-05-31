//go:build linux

package lnx

import (
	"bufio"
	"fmt"
	"net"
	"os"
	"strings"
	"sync"
)

// firecrackerVsock implements VsockDevice using Firecracker's hybrid vsock
// Unix domain socket protocol.
//
// Guest → Host (Listen): Firecracker creates a connection to a Unix socket at
// <udsPath>_<port> when the guest connects to CID 2 on that port.
//
// Host → Guest (Connect): The host dials <udsPath>, sends "CONNECT <port>\n",
// and reads "OK <port>\n" to establish a connection to the guest.
type firecrackerVsock struct {
	udsPath string

	mu        sync.Mutex
	listeners map[uint32]*fcVsockListener
}

func newFirecrackerVsock(udsPath string) *firecrackerVsock {
	return &firecrackerVsock{
		udsPath:   udsPath,
		listeners: make(map[uint32]*fcVsockListener),
	}
}

func (f *firecrackerVsock) Listen(port uint32) (net.Listener, error) {
	sockPath := fmt.Sprintf("%s_%d", f.udsPath, port)

	// Remove any stale socket file.
	os.Remove(sockPath)

	ln, err := net.Listen("unix", sockPath)
	if err != nil {
		return nil, fmt.Errorf("listen %s: %w", sockPath, err)
	}

	fcl := &fcVsockListener{
		Listener: ln,
		sockPath: sockPath,
	}

	f.mu.Lock()
	f.listeners[port] = fcl
	f.mu.Unlock()

	return fcl, nil
}

func (f *firecrackerVsock) Connect(port uint32) (net.Conn, error) {
	conn, err := net.Dial("unix", f.udsPath)
	if err != nil {
		return nil, fmt.Errorf("dial vsock %s: %w", f.udsPath, err)
	}

	// Send CONNECT request.
	if _, err := fmt.Fprintf(conn, "CONNECT %d\n", port); err != nil {
		conn.Close()
		return nil, fmt.Errorf("vsock connect handshake write: %w", err)
	}

	// Read OK response. Firecracker responds with "OK <port>\n" where
	// the port may differ from what was requested (it's the guest-side port).
	reader := bufio.NewReader(conn)
	line, err := reader.ReadString('\n')
	if err != nil {
		conn.Close()
		return nil, fmt.Errorf("vsock connect handshake read: %w", err)
	}
	line = strings.TrimSpace(line)
	if !strings.HasPrefix(line, "OK ") {
		conn.Close()
		return nil, fmt.Errorf("vsock connect: expected OK response, got %q", line)
	}

	// Wrap to handle any buffered data in the reader.
	return &bufferedConn{Conn: conn, reader: reader}, nil
}

// cleanup removes all listener socket files.
func (f *firecrackerVsock) cleanup() {
	f.mu.Lock()
	defer f.mu.Unlock()
	for _, l := range f.listeners {
		l.Close()
	}
}

// fcVsockListener wraps a net.Listener and cleans up the socket file on close.
type fcVsockListener struct {
	net.Listener
	sockPath string
}

func (l *fcVsockListener) Close() error {
	err := l.Listener.Close()
	os.Remove(l.sockPath)
	return err
}

// bufferedConn wraps a net.Conn with a bufio.Reader so that any data
// buffered during the handshake isn't lost.
type bufferedConn struct {
	net.Conn
	reader *bufio.Reader
}

func (c *bufferedConn) Read(p []byte) (int, error) {
	return c.reader.Read(p)
}
