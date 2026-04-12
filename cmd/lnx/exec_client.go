package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/semistrict/lnx"
	"golang.org/x/term"
	"nhooyr.io/websocket"
)

var errExecTerminatedUnexpectedly = errors.New("exec terminated unexpectedly")

// execNonInteractive runs a non-interactive command via POST /exec with NDJSON streaming.
func execNonInteractive(args []string) (int, error) {
	env, err := execEnv()
	if err != nil {
		return -1, err
	}

	body, err := json.Marshal(lnx.ExecRequest{Args: args, Env: env, ClientPID: os.Getpid()})
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

	// Track fork children so we can wait for all of them.
	ft := &forkTracker{}
	ctx := context.Background()

	scanner := bufio.NewScanner(resp.Body)
	scanner.Buffer(make([]byte, 1024*1024), 1024*1024)

	exitCode := -1
	sawExitCode := false
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
		if raw, ok := msg["fork"]; ok {
			var forkInfo struct {
				Instance string `json:"instance"`
			}
			json.Unmarshal(raw, &forkInfo)
			ft.wg.Add(1)
			go attachToForkChild(ctx, forkInfo.Instance, ft)
		}
		if raw, ok := msg["exit_code"]; ok {
			json.Unmarshal(raw, &exitCode)
			sawExitCode = true
		}
	}

	// Wait for all fork children to finish.
	ft.wg.Wait()

	if err := scanner.Err(); err != nil {
		return -1, fmt.Errorf("read exec stream: %w", err)
	}
	if !sawExitCode || exitCode < 0 {
		return -1, errExecTerminatedUnexpectedly
	}
	return exitCode, nil
}

// forkTracker manages child fork connections so the outermost CLI can
// wait for all descendants and broadcast SIGWINCH to them.
type forkTracker struct {
	mu    sync.Mutex
	wg    sync.WaitGroup
	conns []*websocket.Conn // active child WebSocket connections
}

func (ft *forkTracker) add(ws *websocket.Conn) {
	ft.mu.Lock()
	ft.conns = append(ft.conns, ws)
	ft.mu.Unlock()
}

func (ft *forkTracker) remove(ws *websocket.Conn) {
	ft.mu.Lock()
	for i, c := range ft.conns {
		if c == ws {
			ft.conns = append(ft.conns[:i], ft.conns[i+1:]...)
			break
		}
	}
	ft.mu.Unlock()
}

func (ft *forkTracker) broadcastResize(ctx context.Context, data []byte) {
	ft.mu.Lock()
	conns := append([]*websocket.Conn{}, ft.conns...)
	ft.mu.Unlock()
	for _, c := range conns {
		c.Write(ctx, websocket.MessageText, data)
	}
}

// execInteractive runs an interactive command via WebSocket.
func execInteractive(args []string) (int, error) {
	env, err := execEnv()
	if err != nil {
		return -1, err
	}

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
		Env:       env,
		PTY:       true,
		Rows:      rows,
		Cols:      cols,
		ClientPID: os.Getpid(),
	})
	if err := ws.Write(ctx, websocket.MessageText, reqJSON); err != nil {
		return -1, fmt.Errorf("send exec request: %w", err)
	}

	// Track fork children so we can wait for all of them and broadcast resize.
	ft := &forkTracker{}

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
					ft.broadcastResize(ctx, data)
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

	// Read WebSocket messages: binary = PTY output, text = exit_code/fork.
	exitCode := -1
	sawExitCode := false
	var last byte
	var prev byte
	sawOutput := false
	for {
		typ, data, err := ws.Read(ctx)
		if err != nil {
			break
		}
		switch typ {
		case websocket.MessageBinary:
			os.Stdout.Write(data)
			if len(data) > 0 {
				sawOutput = true
				if len(data) >= 2 {
					prev = data[len(data)-2]
				}
				last = data[len(data)-1]
			}
		case websocket.MessageText:
			var msg map[string]json.RawMessage
			if err := json.Unmarshal(data, &msg); err == nil {
				if raw, ok := msg["fork"]; ok {
					var forkInfo struct {
						Instance string `json:"instance"`
					}
					json.Unmarshal(raw, &forkInfo)
					ft.wg.Add(1)
					go attachToForkChild(ctx, forkInfo.Instance, ft)
				}
				if raw, ok := msg["exit_code"]; ok {
					json.Unmarshal(raw, &exitCode)
					sawExitCode = true
				}
			}
		}
	}

	// Wait for all fork children (recursively) to finish.
	ft.wg.Wait()

	// If double Ctrl-C was detected, always return 130.
	select {
	case <-forceQuit:
		return 130, nil
	default:
	}

	if sawOutput && last == '\n' && prev != '\r' {
		_, _ = os.Stdout.Write([]byte{'\r'})
	}

	if !sawExitCode || exitCode < 0 {
		return -1, errExecTerminatedUnexpectedly
	}
	return exitCode, nil
}

// attachToForkChild connects to a forked child VM's fork session and
// multiplexes its PTY output to stdout. Supports recursive forks.
func attachToForkChild(ctx context.Context, instance string, ft *forkTracker) {
	defer ft.wg.Done()

	client := apiClientFor(instance)

	// Wait for the child VM to be reachable, then connect to the fork
	// session. Use a short timeout — if the fork attach server isn't
	// there, CRIU restore likely failed and we shouldn't hang.
	var cws *websocket.Conn
	var err error
	for i := 0; i < 50; i++ {
		cws, _, err = websocket.Dial(ctx, "ws://localhost/fork/ws", &websocket.DialOptions{
			HTTPClient: client,
		})
		if err == nil {
			break
		}
		// Once the child's status socket is reachable but /fork/ws fails
		// with a real HTTP error (not connection refused), the fork session
		// doesn't exist — bail immediately.
		if i > 10 && err != nil && !isNoVM(err) {
			break
		}
		time.Sleep(200 * time.Millisecond)
	}
	if cws == nil {
		slog.Debug("fork attach failed", "instance", instance, "error", err)
		return
	}
	slog.Debug("fork attach connected", "instance", instance)
	defer cws.CloseNow()
	cws.SetReadLimit(-1)

	ft.add(cws)
	defer ft.remove(cws)

	// Send request with current terminal dimensions.
	var rows, cols uint16
	if w, h, err := term.GetSize(int(os.Stdin.Fd())); err == nil {
		rows = uint16(h)
		cols = uint16(w)
	}
	reqJSON, _ := json.Marshal(lnx.ExecRequest{
		PTY:       true,
		Rows:      rows,
		Cols:      cols,
		ClientPID: os.Getpid(),
	})
	if err := cws.Write(ctx, websocket.MessageText, reqJSON); err != nil {
		slog.Debug("failed to send fork attach request", "instance", instance, "error", err)
		return
	}

	// Read child output → stdout, handle recursive forks.
	for {
		typ, data, err := cws.Read(ctx)
		if err != nil {
			break
		}
		switch typ {
		case websocket.MessageBinary:
			os.Stdout.Write(data)
		case websocket.MessageText:
			var msg map[string]json.RawMessage
			if err := json.Unmarshal(data, &msg); err == nil {
				if raw, ok := msg["fork"]; ok {
					var forkInfo struct {
						Instance string `json:"instance"`
					}
					json.Unmarshal(raw, &forkInfo)
					ft.wg.Add(1)
					go attachToForkChild(ctx, forkInfo.Instance, ft)
				}
				if _, ok := msg["exit_code"]; ok {
					return
				}
			}
		}
	}
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
