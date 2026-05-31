package lnx

import (
	"errors"
	"net"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"
)

type testLogListener struct {
	conns   chan net.Conn
	closeCh chan struct{}
	once    sync.Once
}

func newTestLogListener() *testLogListener {
	return &testLogListener{
		conns:   make(chan net.Conn, 8),
		closeCh: make(chan struct{}),
	}
}

func (l *testLogListener) Accept() (net.Conn, error) {
	select {
	case conn := <-l.conns:
		return conn, nil
	case <-l.closeCh:
		return nil, net.ErrClosed
	}
}

func (l *testLogListener) Close() error {
	l.once.Do(func() { close(l.closeCh) })
	return nil
}

type stuckLogListener struct{}

func (stuckLogListener) Accept() (net.Conn, error) {
	select {}
}

func (stuckLogListener) Close() error { return nil }

func TestStartLogReceiverWritesMultipleConnections(t *testing.T) {
	dir := t.TempDir()
	listener := newTestLogListener()
	cleanup := startLogReceiver(listener, dir)
	t.Cleanup(cleanup)

	for _, text := range []string{"first line\n", "second line\n"} {
		server, client := net.Pipe()
		listener.conns <- server
		if _, err := client.Write([]byte(text)); err != nil {
			t.Fatalf("write log line: %v", err)
		}
		_ = client.Close()
	}

	deadline := time.Now().Add(2 * time.Second)
	logPath := filepath.Join(dir, "lnx.log")
	for time.Now().Before(deadline) {
		data, err := os.ReadFile(logPath)
		if err == nil {
			content := string(data)
			if strings.Contains(content, "first line") && strings.Contains(content, "second line") {
				return
			}
		} else if !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("read log file: %v", err)
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatalf("timed out waiting for log output in %s", logPath)
}

func TestStartLogReceiverCleanupDoesNotHangIfAcceptIgnoresClose(t *testing.T) {
	dir := t.TempDir()
	oldTimeout := logReceiverShutdownTimeout
	logReceiverShutdownTimeout = 20 * time.Millisecond
	defer func() { logReceiverShutdownTimeout = oldTimeout }()

	cleanup := startLogReceiver(stuckLogListener{}, dir)

	start := time.Now()
	cleanup()
	if d := time.Since(start); d > 500*time.Millisecond {
		t.Fatalf("cleanup took too long: %v", d)
	}
}
