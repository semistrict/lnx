//go:build darwin

package lnx

import (
	"bufio"
	"fmt"
	"net"
	"os"
	"strings"
	"sync"
	"time"
)

// qemuVsock implements VsockDevice using the vsock Unix domain socket
// protocol exposed by QEMU's virtio-vsock device (socket-path mode).
//
// Guest → Host (Listen): QEMU connects to a Unix socket at
// <udsPath>_<port> when the guest connects to CID 2 on that port.
//
// Host → Guest (Connect): The host dials <udsPath>, sends
// "CONNECT <port>\n", and reads "OK <port>\n" once the guest accepts.
type qemuVsock struct {
	udsPath string

	mu        sync.Mutex
	listeners map[uint32]*qemuVsockListener
}

func newQemuVsock(udsPath string) *qemuVsock {
	return &qemuVsock{
		udsPath:   udsPath,
		listeners: make(map[uint32]*qemuVsockListener),
	}
}

func (q *qemuVsock) Listen(port uint32) (net.Listener, error) {
	sockPath := fmt.Sprintf("%s_%d", q.udsPath, port)

	os.Remove(sockPath)

	ln, err := net.Listen("unix", sockPath)
	if err != nil {
		return nil, fmt.Errorf("listen %s: %w", sockPath, err)
	}

	ql := &qemuVsockListener{
		Listener: ln,
		sockPath: sockPath,
	}

	q.mu.Lock()
	q.listeners[port] = ql
	q.mu.Unlock()

	return ql, nil
}

func (q *qemuVsock) Connect(port uint32) (net.Conn, error) {
	conn, err := net.Dial("unix", q.udsPath)
	if err != nil {
		return nil, fmt.Errorf("dial vsock %s: %w", q.udsPath, err)
	}

	conn.SetDeadline(time.Now().Add(10 * time.Second))
	defer conn.SetDeadline(time.Time{}) // clear deadline after handshake

	if _, err := fmt.Fprintf(conn, "CONNECT %d\n", port); err != nil {
		conn.Close()
		return nil, fmt.Errorf("vsock connect handshake write: %w", err)
	}

	// Wait for "OK <port>\n" — sent by QEMU once the guest accepts.
	reader := bufio.NewReader(conn)
	line, err := reader.ReadString('\n')
	if err != nil {
		conn.Close()
		return nil, fmt.Errorf("vsock connect handshake read: %w", err)
	}
	line = strings.TrimSpace(line)
	if !strings.HasPrefix(line, "OK ") {
		conn.Close()
		return nil, fmt.Errorf("vsock connect: expected OK, got %q", line)
	}

	return &qemuBufferedConn{Conn: conn, reader: reader}, nil
}

func (q *qemuVsock) cleanup() {
	q.mu.Lock()
	defer q.mu.Unlock()
	for _, l := range q.listeners {
		l.Close()
	}
}

type qemuVsockListener struct {
	net.Listener
	sockPath string
}

func (l *qemuVsockListener) Close() error {
	err := l.Listener.Close()
	os.Remove(l.sockPath)
	return err
}

type qemuBufferedConn struct {
	net.Conn
	reader *bufio.Reader
}

func (c *qemuBufferedConn) Read(p []byte) (int, error) {
	return c.reader.Read(p)
}
