package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"
	"time"

	"github.com/semistrict/lnx"
	"golang.org/x/term"
	"nhooyr.io/websocket"
)

// execNonInteractive runs a non-interactive command via POST /exec with NDJSON streaming.
func execNonInteractive(args []string) (int, error) {
	body, err := json.Marshal(lnx.ExecRequest{Args: args, ClientPID: os.Getpid()})
	if err != nil {
		return -1, err
	}

	resp, err := apiClient().Post("http://localhost/exec", "application/json", bytes.NewReader(body))
	if err != nil {
		if isNoVM(err) {
			return -1, noVMError()
		}
		return -1, fmt.Errorf("connect to VM: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		var buf bytes.Buffer
		buf.ReadFrom(resp.Body)
		return -1, fmt.Errorf("exec failed: %s", strings.TrimSpace(buf.String()))
	}

	// Catch signals — closing the response body terminates the HTTP stream,
	// which closes the exec connection and cleans up the guest process.
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
	defer signal.Stop(sigCh)
	go func() {
		<-sigCh
		resp.Body.Close()
	}()

	scanner := bufio.NewScanner(resp.Body)
	scanner.Buffer(make([]byte, 1024*1024), 1024*1024)

	exitCode := -1
	for scanner.Scan() {
		line := scanner.Bytes()
		var msg map[string]json.RawMessage
		if err := json.Unmarshal(line, &msg); err != nil {
			continue
		}
		if raw, ok := msg["stdout"]; ok {
			var s string
			json.Unmarshal(raw, &s)
			os.Stdout.WriteString(s)
		}
		if raw, ok := msg["stderr"]; ok {
			var s string
			json.Unmarshal(raw, &s)
			os.Stderr.WriteString(s)
		}
		if raw, ok := msg["exit_code"]; ok {
			json.Unmarshal(raw, &exitCode)
		}
	}
	return exitCode, nil
}

// execInteractive runs an interactive command via WebSocket.
func execInteractive(args []string) (int, error) {
	fd := int(os.Stdin.Fd())
	var rows, cols uint16
	if term.IsTerminal(fd) {
		w, h, err := term.GetSize(fd)
		if err == nil {
			rows = uint16(h)
			cols = uint16(w)
		}
		oldState, err := term.MakeRaw(fd)
		if err == nil {
			defer term.Restore(fd, oldState)
		}
	}

	ctx := context.Background()
	ws, _, err := websocket.Dial(ctx, "ws://localhost/exec/ws", &websocket.DialOptions{
		HTTPClient: apiClient(),
	})
	if err != nil {
		if isNoVM(err) {
			return -1, noVMError()
		}
		return -1, fmt.Errorf("connect to VM: %w", err)
	}
	defer ws.CloseNow()
	ws.SetReadLimit(-1) // no limit on PTY data

	// Send exec request as first text message.
	reqJSON, _ := json.Marshal(lnx.ExecRequest{
		Args:      args,
		PTY:       true,
		Rows:      rows,
		Cols:      cols,
		ClientPID: os.Getpid(),
	})
	if err := ws.Write(ctx, websocket.MessageText, reqJSON); err != nil {
		return -1, fmt.Errorf("send exec request: %w", err)
	}

	// Forward host signals (SIGWINCH, SIGINT, SIGTERM, SIGHUP) to the guest
	// via WebSocket text frames.
	sigCh := make(chan os.Signal, 4)
	signal.Notify(sigCh, syscall.SIGWINCH, syscall.SIGINT, syscall.SIGTERM, syscall.SIGHUP)
	defer signal.Stop(sigCh)

	go func() {
		for sig := range sigCh {
			if sig == syscall.SIGWINCH {
				w, h, err := term.GetSize(fd)
				if err == nil {
					data, _ := json.Marshal(map[string]any{
						"resize": map[string]uint16{"rows": uint16(h), "cols": uint16(w)},
					})
					ws.Write(ctx, websocket.MessageText, data)
				}
			} else {
				data, _ := json.Marshal(map[string]any{
					"signal": int(sig.(syscall.Signal)),
				})
				ws.Write(ctx, websocket.MessageText, data)
			}
		}
	}()

	// Read stdin → binary WebSocket frames (with double Ctrl-C detection).
	forceQuit := make(chan struct{})
	go func() {
		buf := make([]byte, 32*1024)
		var lastCtrlC time.Time
		for {
			n, err := os.Stdin.Read(buf)
			if n > 0 {
				// Detect double Ctrl-C.
				for i := 0; i < n; i++ {
					if buf[i] == 0x03 {
						now := time.Now()
						if !lastCtrlC.IsZero() && now.Sub(lastCtrlC) < time.Second {
							fmt.Fprintln(os.Stderr, "\r\nforce quit")
							close(forceQuit)
							ws.Close(websocket.StatusNormalClosure, "force quit")
							return
						}
						lastCtrlC = now
					}
				}
				ws.Write(ctx, websocket.MessageBinary, buf[:n])
			}
			if err != nil {
				return
			}
		}
	}()

	// Read WebSocket messages: binary = PTY output, text = exit_code.
	exitCode := -1
	for {
		typ, data, err := ws.Read(ctx)
		if err != nil {
			break
		}
		switch typ {
		case websocket.MessageBinary:
			os.Stdout.Write(data)
		case websocket.MessageText:
			var msg map[string]json.RawMessage
			if err := json.Unmarshal(data, &msg); err == nil {
				if raw, ok := msg["exit_code"]; ok {
					json.Unmarshal(raw, &exitCode)
				}
			}
		}
	}

	// If double Ctrl-C was detected, always return 130.
	select {
	case <-forceQuit:
		return 130, nil
	default:
	}

	return exitCode, nil
}

// waitForVM polls status.sock until the daemon is ready, up to timeout.
// If the daemon exits with an error, it reads error.log for diagnostics.
func waitForVM(timeout time.Duration) error {
	sockPaths := statusSockPaths()
	// Only check error.log paths the daemon can write to.
	// For nested instances (LNX_PARENT set), the instance dir may be
	// on a read-only mount with stale logs from previous attempts.
	qname := qualifiedInstanceName()
	var errPaths []string
	if os.Getenv("LNX_PARENT") != "" {
		errPaths = []string{filepath.Join("/var/lib/lnx/instances", qname, "error.log")}
	} else {
		errPaths = []string{filepath.Join(instanceDir(), "error.log")}
	}

	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		for _, sp := range sockPaths {
			conn, err := net.DialTimeout("unix", sp, 500*time.Millisecond)
			if err == nil {
				conn.Close()
				return nil
			}
		}
		if msg := readFirstErrorLog(errPaths); msg != "" {
			return fmt.Errorf("VM failed to start: %s", msg)
		}
		time.Sleep(200 * time.Millisecond)
	}
	if msg := readFirstErrorLog(errPaths); msg != "" {
		return fmt.Errorf("VM failed to start: %s", msg)
	}
	return fmt.Errorf("timed out waiting for VM to start")
}

func readFirstErrorLog(paths []string) string {
	for _, p := range paths {
		if data, err := os.ReadFile(p); err == nil && len(data) > 0 {
			return strings.TrimSpace(string(data))
		}
	}
	return ""
}
