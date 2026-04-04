//go:build darwin && integration

package lnx_test

import (
	"fmt"
	"io"
	"net"
	"testing"
	"time"

	"github.com/semistrict/lnx"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestRun_PortForward(t *testing.T) {
	t.Parallel()
	dir := setupTestDir(t)
	cfg := testConfig(dir)

	// Use a shell-based TCP listener that's available everywhere.
	// bash's /dev/tcp doesn't work for listening, so use python3.
	guestPort := 9876
	cmd := fmt.Sprintf(
		`python3 -c "
import socket, sys
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('0.0.0.0', %d))
s.listen(1)
conn, _ = s.accept()
conn.sendall(b'HELLO_FROM_GUEST\n')
conn.close()
s.close()
"`, guestPort,
	)

	errCh := make(chan error, 1)
	codeCh := make(chan int, 1)
	go func() {
		code, err := lnx.Run(cfg, "sh", "-c", cmd)
		errCh <- err
		codeCh <- code
	}()

	// Wait for port forwarding to detect the listener and forward it.
	var conn net.Conn
	var err error
	for i := 0; i < 30; i++ {
		time.Sleep(time.Second)
		conn, err = net.DialTimeout("tcp", fmt.Sprintf("127.0.0.1:%d", guestPort), time.Second)
		if err == nil {
			break
		}
	}
	require.NoError(t, err, "failed to connect to forwarded port %d", guestPort)

	data, err := io.ReadAll(conn)
	conn.Close()
	require.NoError(t, err)
	assert.Contains(t, string(data), "HELLO_FROM_GUEST")

	require.NoError(t, <-errCh)
	assert.Equal(t, 0, <-codeCh)
}
