//go:build linux

package lnx

import (
	"fmt"
	"net"
	"os"
	"path/filepath"
	"testing"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestFirecrackerVsock_Listen(t *testing.T) {
	dir := t.TempDir()
	udsPath := filepath.Join(dir, "vsock")

	vsock := newFirecrackerVsock(udsPath)

	ln, err := vsock.Listen(1024)
	require.NoError(t, err)
	defer ln.Close()

	// The socket file should exist at udsPath_1024.
	sockPath := fmt.Sprintf("%s_%d", udsPath, 1024)
	_, err = os.Stat(sockPath)
	assert.NoError(t, err)
}

func TestFirecrackerVsock_Connect(t *testing.T) {
	dir := t.TempDir()
	udsPath := filepath.Join(dir, "vsock")

	// Create a mock Firecracker vsock server that handles CONNECT.
	mockLn, err := net.Listen("unix", udsPath)
	require.NoError(t, err)
	defer mockLn.Close()

	go func() {
		conn, err := mockLn.Accept()
		if err != nil {
			return
		}
		defer conn.Close()

		buf := make([]byte, 256)
		n, err := conn.Read(buf)
		if err != nil {
			return
		}

		// Expect "CONNECT 1027\n"
		request := string(buf[:n])
		assert.Equal(t, "CONNECT 1027\n", request)

		// Respond with "OK 1027\n"
		fmt.Fprintf(conn, "OK 1027\n")

		// Echo back any data received.
		for {
			n, err := conn.Read(buf)
			if err != nil {
				return
			}
			conn.Write(buf[:n])
		}
	}()

	vsock := newFirecrackerVsock(udsPath)

	conn, err := vsock.Connect(1027)
	require.NoError(t, err)
	defer conn.Close()

	// Verify we can send and receive data.
	_, err = conn.Write([]byte("hello"))
	require.NoError(t, err)

	buf := make([]byte, 32)
	n, err := conn.Read(buf)
	require.NoError(t, err)
	assert.Equal(t, "hello", string(buf[:n]))
}

func TestFirecrackerVsock_ListenCleanup(t *testing.T) {
	dir := t.TempDir()
	udsPath := filepath.Join(dir, "vsock")

	vsock := newFirecrackerVsock(udsPath)

	ln, err := vsock.Listen(1025)
	require.NoError(t, err)

	sockPath := fmt.Sprintf("%s_%d", udsPath, 1025)
	_, err = os.Stat(sockPath)
	require.NoError(t, err)

	ln.Close()

	// Socket file should be removed after close.
	_, err = os.Stat(sockPath)
	assert.True(t, os.IsNotExist(err))
}
