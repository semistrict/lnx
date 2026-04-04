package lnx

import (
	"bufio"
	"fmt"
	"log/slog"
	"net"
	"os"
	"path/filepath"
)

// startLogReceiver listens on vsockLogPort and appends received log lines
// to ~/.lnx/lnx.log. Returns a cleanup function.
func startLogReceiver(listener interface {
	Accept() (net.Conn, error)
	Close() error
}, logDir string) func() {
	logPath := filepath.Join(logDir, "lnx.log")
	done := make(chan struct{})

	go func() {
		defer close(done)

		conn, err := listener.Accept()
		if err != nil {
			slog.Debug("log accept failed", "error", err)
			return
		}
		slog.Debug("log accepted")
		defer conn.Close()

		f, err := os.OpenFile(logPath, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0644)
		if err != nil {
			fmt.Fprintf(os.Stderr, "lnx: open log file: %v\n", err)
			return
		}
		defer f.Close()

		scanner := bufio.NewScanner(conn)
		for scanner.Scan() {
			fmt.Fprintln(f, scanner.Text())
		}
	}()

	return func() {
		<-done
	}
}
