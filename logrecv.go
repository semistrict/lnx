package lnx

import (
	"bufio"
	"fmt"
	"log/slog"
	"net"
	"os"
	"path/filepath"
	"sync"
	"time"
)

var logReceiverShutdownTimeout = time.Second

// startLogReceiver listens on vsockLogPort and appends received log lines
// to ~/.lnx/lnx.log. Returns a cleanup function.
func startLogReceiver(listener interface {
	Accept() (net.Conn, error)
	Close() error
}, logDir string) func() {
	logPath := filepath.Join(logDir, "lnx.log")
	done := make(chan struct{})
	var wg sync.WaitGroup

	go func() {
		f, err := os.OpenFile(logPath, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0644)
		if err != nil {
			fmt.Fprintf(os.Stderr, "lnx: open log file: %v\n", err)
			close(done)
			return
		}
		defer f.Close()
		defer close(done)

		var mu sync.Mutex
		for {
			conn, err := listener.Accept()
			if err != nil {
				slog.Debug("log accept failed", "error", err)
				break
			}
			slog.Debug("log accepted")
			wg.Add(1)
			go func(conn net.Conn) {
				defer wg.Done()
				defer conn.Close()

				scanner := bufio.NewScanner(conn)
				for scanner.Scan() {
					mu.Lock()
					fmt.Fprintln(f, scanner.Text())
					mu.Unlock()
				}
			}(conn)
		}
		wg.Wait()
	}()

	return func() {
		_ = listener.Close()
		select {
		case <-done:
		case <-time.After(logReceiverShutdownTimeout):
			slog.Warn("log receiver shutdown timed out")
		}
	}
}
