//go:build linux

package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/gob"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/mdlayher/vsock"
	"github.com/semistrict/lnx"
	"github.com/semistrict/lnx/internal/protocol"
	"nhooyr.io/websocket"
)

const (
	memorySnapshotRuntimeDir = "/var/lib/lnx/memorysnapshot"
	innerSocketDir           = memorySnapshotRuntimeDir + "/inner"
	innerRootfsPath          = memorySnapshotRuntimeDir + "/rootfs.ext4"
	innerSwapPath            = memorySnapshotRuntimeDir + "/swap.img"
	innerRunSocketBase       = "/var/run/lnx"
)

func memorySnapshotModeEnabled() bool {
	return os.Getenv("LNX_TOPLEVEL_MODE") == "memorysnapshot" || lnx.ExperimentEnabled("memorysnapshot")
}

func runMemorySnapshotMode(setup *protocol.Setup) error {
	instanceName := os.Getenv("LNX_TOPLEVEL_INSTANCE")
	if instanceName == "" {
		instanceName = strings.TrimSuffix(setup.Hostname, ".lnx")
	}
	base := os.Getenv("LNX_BASE")
	if base == "" {
		base = filepath.Join(setup.HomeDir, ".lnx")
	}
	instanceDir := filepath.Join(base, "instances", instanceName)
	errorPath := filepath.Join(instanceDir, "memorysnapshot", "boot-error.log")
	_ = os.MkdirAll(filepath.Dir(errorPath), 0755)
	_ = os.Remove(errorPath)
	slog.Info("memorysnapshot wrapper starting", "instance", instanceName, "instance_dir", instanceDir)
	if err := os.MkdirAll(memorySnapshotRuntimeDir, 0755); err != nil {
		return fmt.Errorf("create memorysnapshot runtime dir: %w", err)
	}
	if err := os.MkdirAll(innerSocketDir, 0755); err != nil {
		return fmt.Errorf("create inner socket dir: %w", err)
	}

	snapshotDir, snapshotCfg, err := prepareInnerRuntime(instanceDir)
	if err != nil {
		return err
	}
	slog.Info("memorysnapshot inner runtime prepared", "snapshot_dir", snapshotDir, "restore", snapshotCfg != nil)

	initBytes, err := os.ReadFile("/proc/self/exe")
	if err != nil {
		return fmt.Errorf("read guest init binary for inner daemon: %w", err)
	}
	lnx.InitBinary = initBytes
	slog.Info("memorysnapshot captured inner init binary", "bytes", len(initBytes))

	prevExperiments := os.Getenv("LNX_EXPERIMENTS")
	if !lnx.ExperimentEnabled("linux_host") {
		if prevExperiments == "" {
			_ = os.Setenv("LNX_EXPERIMENTS", "linux_host")
		} else {
			_ = os.Setenv("LNX_EXPERIMENTS", prevExperiments+",linux_host")
		}
	}
	defer func() { _ = os.Setenv("LNX_EXPERIMENTS", prevExperiments) }()
	prevHome := os.Getenv("HOME")
	_ = os.Setenv("HOME", setup.HomeDir)
	defer func() { _ = os.Setenv("HOME", prevHome) }()

	cfg := &lnx.Config{
		KernelPath:     resolveInnerKernel(base),
		RootfsPath:     innerRootfsPath,
		SocketDir:      innerSocketDir,
		InstanceDir:    instanceDir,
		MemorySnapshot: snapshotCfg,
		CWD:            setup.CWD,
		Env:            append(filterInnerEnv(setup.Env), "LNX_EXPERIMENTS=linux_host", "LNX_PERSIST_ON_CONTROL_DROP=1"),
		User:           setup.User,
		UID:            setup.UID,
		HomeDir:        setup.HomeDir,
		Hostname:       setup.Hostname,
		SSHAgent:       setup.SSHAgent,
		Shares:         setup.Shares,
	}
	slog.Info("memorysnapshot launching inner daemon", "kernel", cfg.KernelPath, "rootfs", cfg.RootfsPath, "socket_dir", cfg.SocketDir)

	innerErrCh := make(chan error, 1)
	go func() {
		innerErrCh <- lnx.RunDaemon(cfg)
	}()

	innerSock := innerStatusSockPath(cfg.SocketDir)
	slog.Info("memorysnapshot waiting for inner ready", "socket", innerSock)
	if err := waitForInnerReadyOrExit(innerSock, innerErrCh, 60*time.Second); err != nil {
		_ = os.WriteFile(errorPath, []byte(err.Error()+"\n"), 0644)
		return fmt.Errorf("wait for inner daemon ready: %w", err)
	}
	_ = os.Remove(errorPath)
	slog.Info("memorysnapshot inner daemon ready", "socket", innerSock)

	if snapshotDir != "" {
		_ = os.WriteFile(filepath.Join(snapshotDir, "consumed"), []byte("1\n"), 0644)
	}

	startStatusProxy(innerSock)
	startExecProxy(innerSock)
	startGuestControlProxy(innerSock)
	startPortForwarder()

	select {
	case <-ctrlDone:
	case err := <-innerErrCh:
		if err != nil {
			_ = os.WriteFile(errorPath, []byte(err.Error()+"\n"), 0644)
			return fmt.Errorf("inner daemon: %w", err)
		}
		_ = os.WriteFile(errorPath, []byte("inner daemon exited unexpectedly\n"), 0644)
		return fmt.Errorf("inner daemon exited unexpectedly")
	}

	if err := stopInnerDaemon(innerSock); err != nil {
		slog.Warn("stop inner daemon", "error", err)
	}
	if snapshotDir != "" {
		_ = os.RemoveAll(snapshotDir)
	}
	return nil
}

func resolveInnerKernel(base string) string {
	fcKernel := filepath.Join(base, "vmlinuz-firecracker")
	if _, err := os.Stat(fcKernel); err == nil {
		return fcKernel
	}
	return filepath.Join(base, "vmlinuz")
}

func effectiveInnerSocketDir(sockDir string) string {
	return filepath.Join(innerRunSocketBase, filepath.Base(sockDir))
}

func innerStatusSockPath(sockDir string) string {
	return filepath.Join(effectiveInnerSocketDir(sockDir), "status.sock")
}

func prepareInnerRuntime(instanceDir string) (string, *lnx.MemorySnapshot, error) {
	rootfsTarget := filepath.Join(instanceDir, "rootfs.ext4")
	if err := recreateSymlink(innerRootfsPath, rootfsTarget); err != nil {
		return "", nil, fmt.Errorf("link inner rootfs: %w", err)
	}

	currentSnapshotDir := filepath.Join(instanceDir, "memorysnapshot", "current")
	if _, err := os.Stat(filepath.Join(currentSnapshotDir, "vmstate.bin")); err != nil {
		_ = os.Remove(innerSwapPath)
		return "", nil, nil
	}
	if _, err := os.Stat(filepath.Join(currentSnapshotDir, "consumed")); err == nil {
		_ = os.Remove(innerSwapPath)
		return "", nil, nil
	}
	swapTarget := filepath.Join(currentSnapshotDir, "swap.img")
	if _, err := os.Stat(swapTarget); err == nil {
		if err := recreateSymlink(innerSwapPath, swapTarget); err != nil {
			return "", nil, fmt.Errorf("link inner swap: %w", err)
		}
	} else {
		_ = os.Remove(innerSwapPath)
	}
	return currentSnapshotDir, &lnx.MemorySnapshot{
		StatePath: filepath.Join(currentSnapshotDir, "vmstate.bin"),
		MemPath:   filepath.Join(currentSnapshotDir, "memory.bin"),
	}, nil
}

func filterInnerEnv(env []string) []string {
	var out []string
	for _, kv := range env {
		if strings.HasPrefix(kv, "LNX_TOPLEVEL_") || strings.HasPrefix(kv, "LNX_BASE=") {
			continue
		}
		out = append(out, kv)
	}
	return out
}

func recreateSymlink(linkPath, target string) error {
	_ = os.Remove(linkPath)
	if err := os.Symlink(target, linkPath); err != nil {
		return err
	}
	return nil
}

func waitForUnixSocket(path string, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		conn, err := net.Dial("unix", path)
		if err == nil {
			_ = conn.Close()
			return nil
		}
		time.Sleep(100 * time.Millisecond)
	}
	return fmt.Errorf("socket %s did not appear within %v", path, timeout)
}

func waitForUnixSocketOrExit(path string, innerErrCh <-chan error, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		select {
		case err := <-innerErrCh:
			if err != nil {
				return err
			}
			return fmt.Errorf("inner daemon exited before socket appeared")
		default:
		}
		conn, err := net.Dial("unix", path)
		if err == nil {
			_ = conn.Close()
			return nil
		}
		time.Sleep(100 * time.Millisecond)
	}
	return fmt.Errorf("socket %s did not appear within %v", path, timeout)
}

func waitForInnerReadyOrExit(sockPath string, innerErrCh <-chan error, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	start := time.Now()
	lastDetail := ""
	lastLog := time.Time{}
	for time.Now().Before(deadline) {
		select {
		case err := <-innerErrCh:
			if err != nil {
				return err
			}
			return fmt.Errorf("inner daemon exited before becoming ready")
		default:
		}
		stage, err := probeInnerReadyDetailed(sockPath)
		if err == nil {
			if stage != "" {
				slog.Info("memorysnapshot inner ready probe succeeded", "stage", stage, "elapsed", time.Since(start))
			}
			return nil
		}
		detail := stage + ": " + err.Error()
		if detail != lastDetail || time.Since(lastLog) >= 2*time.Second {
			slog.Info("memorysnapshot inner ready pending", "elapsed", time.Since(start), "stage", stage, "error", err)
			lastDetail = detail
			lastLog = time.Now()
		}
		time.Sleep(100 * time.Millisecond)
	}
	return fmt.Errorf("socket %s did not become ready within %v", sockPath, timeout)
}

func innerHTTPClient(sock string) *http.Client {
	return innerHTTPClientTimeout(sock, 0)
}

func innerHTTPClientTimeout(sock string, timeout time.Duration) *http.Client {
	return &http.Client{
		Timeout: timeout,
		Transport: &http.Transport{
			DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
				var d net.Dialer
				return d.DialContext(ctx, "unix", sock)
			},
		},
	}
}

func startStatusProxy(innerSock string) {
	conn, err := vsock.Dial(vsockHostCID, protocol.StatusPort, nil)
	if err != nil {
		slog.Warn("status vsock dial failed", "error", err)
		return
	}

	go func() {
		defer conn.Close()
		enc := gob.NewEncoder(conn)
		dec := gob.NewDecoder(conn)
		for {
			var msg protocol.Msg
			if err := dec.Decode(&msg); err != nil {
				return
			}
			if msg.StatusReq == nil {
				continue
			}
			resp, err := proxyInnerStatus(innerSock, msg.StatusReq.IncludeDmesg)
			if err != nil {
				if readyErr := probeInnerReady(innerSock); readyErr != nil {
					slog.Warn("proxy inner status", "error", err, "ready_error", readyErr)
					return
				}
				resp = protocol.StatusResp{}
			}
			if err := enc.Encode(protocol.Msg{StatusResp: &resp}); err != nil {
				return
			}
		}
	}()
}

func proxyInnerStatus(innerSock string, includeDmesg bool) (protocol.StatusResp, error) {
	url := "http://localhost/status"
	if includeDmesg {
		url += "?dmesg=1"
	}
	resp, err := innerHTTPClient(innerSock).Get(url)
	if err != nil {
		return protocol.StatusResp{}, err
	}
	defer resp.Body.Close()
	if resp.StatusCode/100 != 2 {
		data, _ := io.ReadAll(resp.Body)
		msg := strings.TrimSpace(string(data))
		if msg == "" {
			msg = resp.Status
		}
		return protocol.StatusResp{}, fmt.Errorf("inner status %s", msg)
	}
	var payload lnx.StatusResponse
	if err := json.NewDecoder(resp.Body).Decode(&payload); err != nil {
		return protocol.StatusResp{}, err
	}
	return protocol.StatusResp{
		UptimeSecs:  payload.UptimeSecs,
		MemTotalKB:  payload.MemTotalKB,
		MemAvailKB:  payload.MemAvailKB,
		SwapTotalKB: payload.SwapTotalKB,
		SwapFreeKB:  payload.SwapFreeKB,
		DiskTotalKB: payload.DiskTotalKB,
		DiskUsedKB:  payload.DiskUsedKB,
		LoadAvg:     payload.LoadAvg,
		Dmesg:       payload.Dmesg,
	}, nil
}

func probeInnerReady(innerSock string) error {
	_, err := probeInnerReadyDetailed(innerSock)
	return err
}

func probeInnerReadyDetailed(innerSock string) (string, error) {
	readyStage := "ready_http"
	resp, err := innerHTTPClientTimeout(innerSock, 500*time.Millisecond).Get("http://localhost/ready")
	if err == nil {
		if resp.StatusCode == http.StatusNoContent {
			_ = resp.Body.Close()
			return readyStage, nil
		}
		body, _ := io.ReadAll(io.LimitReader(resp.Body, 4096))
		_ = resp.Body.Close()
		msg := strings.TrimSpace(string(body))
		if msg != "" {
			err = fmt.Errorf("inner /ready returned %s: %s", resp.Status, msg)
		} else {
			err = fmt.Errorf("inner /ready returned %s", resp.Status)
		}
	} else {
		err = fmt.Errorf("inner /ready request failed: %w", err)
	}
	execErr := runInnerProbe(innerSock, "true")
	if execErr == nil {
		return "exec_probe", nil
	}
	return "exec_probe", fmt.Errorf("%v; inner exec probe failed: %w", err, execErr)
}

func runInnerProbe(innerSock string, args ...string) error {
	body, _ := json.Marshal(lnx.ExecRequest{Args: args})
	resp, err := innerHTTPClientTimeout(innerSock, time.Second).Post("http://localhost/exec", "application/json", bytes.NewReader(body))
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode/100 != 2 {
		data, _ := io.ReadAll(resp.Body)
		msg := strings.TrimSpace(string(data))
		if msg == "" {
			msg = resp.Status
		}
		return fmt.Errorf("inner exec probe %s", msg)
	}

	scanner := bufio.NewScanner(resp.Body)
	for scanner.Scan() {
		var line map[string]any
		if err := json.Unmarshal(scanner.Bytes(), &line); err != nil {
			continue
		}
		if exitCode, ok := line["exit_code"].(float64); ok {
			if int(exitCode) != 0 {
				return fmt.Errorf("inner exec probe exit %d", int(exitCode))
			}
			return nil
		}
	}
	if err := scanner.Err(); err != nil {
		return err
	}
	return fmt.Errorf("inner exec probe did not report exit code")
}

func startExecProxy(innerSock string) {
	execLn, err := vsock.Listen(protocol.ExecPort, nil)
	if err != nil {
		slog.Warn("exec proxy listen failed", "error", err)
		return
	}
	interactiveLn, err := vsock.Listen(protocol.ExecInteractivePort, nil)
	if err != nil {
		slog.Warn("exec interactive proxy listen failed", "error", err)
		_ = execLn.Close()
		return
	}

	go func() {
		for {
			conn, err := execLn.Accept()
			if err != nil {
				return
			}
			go handleExecProxyConn(conn.(*vsock.Conn), interactiveLn, innerSock)
		}
	}()
}

func handleExecProxyConn(conn *vsock.Conn, interactiveLn *vsock.Listener, innerSock string) {
	defer conn.Close()
	enc := gob.NewEncoder(conn)
	dec := gob.NewDecoder(conn)

	var msg protocol.Msg
	if err := dec.Decode(&msg); err != nil {
		return
	}
	if msg.ExecReq == nil {
		return
	}
	if msg.ExecReq.PTY {
		runExecProxyPTY(enc, dec, msg.ExecReq, interactiveLn, innerSock)
		return
	}
	runExecProxyPipe(enc, msg.ExecReq, innerSock)
}

func runExecProxyPipe(enc *gob.Encoder, req *protocol.ExecReq, innerSock string) {
	slog.Debug("memorysnapshot exec proxy pipe start", "args", req.Args)
	body, _ := json.Marshal(lnx.ExecRequest{
		Args: req.Args,
		Env:  req.Env,
		PTY:  false,
		Rows: req.Rows,
		Cols: req.Cols,
	})
	resp, err := innerHTTPClient(innerSock).Post("http://localhost/exec", "application/json", bytes.NewReader(body))
	if err != nil {
		slog.Warn("memorysnapshot exec proxy inner post failed", "args", req.Args, "error", err)
		_ = enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
		return
	}
	defer resp.Body.Close()
	slog.Debug("memorysnapshot exec proxy inner post ok", "args", req.Args, "status", resp.StatusCode)

	dec := json.NewDecoder(resp.Body)
	sawExit := false
	for {
		var line map[string]any
		if err := dec.Decode(&line); err != nil {
			if err != io.EOF || !sawExit {
				slog.Warn("memorysnapshot exec proxy decode failed", "args", req.Args, "error", err, "saw_exit", sawExit)
				_ = enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
			}
			return
		}
		if stdout, ok := line["stdout"].(string); ok && stdout != "" {
			slog.Debug("memorysnapshot exec proxy stdout", "args", req.Args, "bytes", len(stdout))
			_ = enc.Encode(protocol.Msg{ExecOutput: &protocol.ExecOutput{Stdout: []byte(stdout)}})
		}
		if stderr, ok := line["stderr"].(string); ok && stderr != "" {
			slog.Debug("memorysnapshot exec proxy stderr", "args", req.Args, "bytes", len(stderr))
			_ = enc.Encode(protocol.Msg{ExecOutput: &protocol.ExecOutput{Stderr: []byte(stderr)}})
		}
		if exitCode, ok := line["exit_code"].(float64); ok {
			slog.Debug("memorysnapshot exec proxy exit", "args", req.Args, "exit_code", int(exitCode))
			sawExit = true
			_ = enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: int(exitCode)}})
			return
		}
	}
}

func runExecProxyPTY(enc *gob.Encoder, dec *gob.Decoder, req *protocol.ExecReq, interactiveLn *vsock.Listener, innerSock string) {
	ctx := context.Background()
	ws, _, err := websocket.Dial(ctx, "ws://localhost/exec/ws", &websocket.DialOptions{
		HTTPClient: innerHTTPClient(innerSock),
	})
	if err != nil {
		_ = enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
		return
	}
	defer ws.CloseNow()
	ws.SetReadLimit(-1)

	reqJSON, _ := json.Marshal(lnx.ExecRequest{
		Args: req.Args,
		Env:  req.Env,
		PTY:  true,
		Rows: req.Rows,
		Cols: req.Cols,
	})
	if err := ws.Write(ctx, websocket.MessageText, reqJSON); err != nil {
		_ = enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
		return
	}

	vsockConn, err := interactiveLn.Accept()
	if err != nil {
		_ = enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
		return
	}
	defer vsockConn.Close()

	go func() {
		for {
			var ctrl protocol.Msg
			if err := dec.Decode(&ctrl); err != nil {
				_ = ws.Close(websocket.StatusNormalClosure, "")
				return
			}
			if ctrl.ExecSignal != nil {
				data, _ := json.Marshal(map[string]any{"signal": ctrl.ExecSignal.Sig})
				_ = ws.Write(ctx, websocket.MessageText, data)
			}
			if ctrl.ExecResize != nil {
				data, _ := json.Marshal(map[string]any{
					"resize": map[string]uint16{"rows": ctrl.ExecResize.Rows, "cols": ctrl.ExecResize.Cols},
				})
				_ = ws.Write(ctx, websocket.MessageText, data)
			}
		}
	}()

	go func() {
		buf := make([]byte, 32*1024)
		for {
			n, err := vsockConn.Read(buf)
			if n > 0 {
				if err := ws.Write(ctx, websocket.MessageBinary, buf[:n]); err != nil {
					return
				}
			}
			if err != nil {
				_ = ws.Close(websocket.StatusNormalClosure, "")
				return
			}
		}
	}()

	for {
		typ, data, err := ws.Read(ctx)
		if err != nil {
			_ = enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
			return
		}
		switch typ {
		case websocket.MessageBinary:
			if _, err := vsockConn.Write(data); err != nil {
				_ = enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: 127}})
				return
			}
		case websocket.MessageText:
			var payload map[string]int
			if err := json.Unmarshal(data, &payload); err == nil {
				if exitCode, ok := payload["exit_code"]; ok {
					_ = enc.Encode(protocol.Msg{ExecDone: &protocol.ExecDone{ExitCode: exitCode}})
					return
				}
			}
		}
	}
}

func startGuestControlProxy(innerSock string) {
	os.MkdirAll("/var/run/lnx", 0755)
	_ = os.Remove(guestControlSock)

	mux := newGuestControlProxyMux(innerSock)

	ln, err := net.Listen("unix", guestControlSock)
	if err == nil {
		_ = os.Chmod(guestControlSock, 0666)
		go http.Serve(ln, mux)
	}

	vsockLn, err := vsock.Listen(protocol.GuestHTTPPort, nil)
	if err == nil {
		go http.Serve(vsockLn, mux)
	}
}

func newGuestControlProxyMux(innerSock string) *http.ServeMux {
	mux := http.NewServeMux()
	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/memorysnapshot/create" && r.Method == http.MethodPost {
			if err := runInnerSync(innerSock); err != nil {
				http.Error(w, err.Error(), http.StatusBadGateway)
				return
			}
		}
		proxyInnerHTTP(w, r, innerSock)
	})
	return mux
}

func proxyInnerHTTP(w http.ResponseWriter, r *http.Request, innerSock string) {
	slog.Info("memorysnapshot proxy inner request", "method", r.Method, "path", r.URL.Path, "inner_sock", innerSock)
	targetURL := "http://localhost" + r.URL.RequestURI()
	req, err := http.NewRequestWithContext(r.Context(), r.Method, targetURL, r.Body)
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	req.Header = r.Header.Clone()
	resp, err := innerHTTPClient(innerSock).Do(req)
	if err != nil {
		slog.Warn("memorysnapshot proxy inner request failed", "method", r.Method, "path", r.URL.Path, "inner_sock", innerSock, "error", err)
		http.Error(w, fmt.Sprintf("inner request failed: %v", err), http.StatusBadGateway)
		return
	}
	defer resp.Body.Close()
	slog.Info("memorysnapshot proxy inner response", "method", r.Method, "path", r.URL.Path, "status", resp.StatusCode)
	for k, vals := range resp.Header {
		for _, v := range vals {
			w.Header().Add(k, v)
		}
	}
	w.WriteHeader(resp.StatusCode)
	_, _ = io.Copy(w, resp.Body)
}

func runInnerSync(innerSock string) error {
	body, _ := json.Marshal(lnx.ExecRequest{Args: []string{"sync"}})
	resp, err := innerHTTPClient(innerSock).Post("http://localhost/exec", "application/json", bytes.NewReader(body))
	if err != nil {
		return fmt.Errorf("sync exec: %w", err)
	}
	defer resp.Body.Close()

	scanner := bufio.NewScanner(resp.Body)
	for scanner.Scan() {
		var line map[string]any
		if err := json.Unmarshal(scanner.Bytes(), &line); err != nil {
			continue
		}
		if exitCode, ok := line["exit_code"].(float64); ok {
			if int(exitCode) != 0 {
				return fmt.Errorf("sync exited with %d", int(exitCode))
			}
			return nil
		}
	}
	if err := scanner.Err(); err != nil {
		return err
	}
	return fmt.Errorf("sync did not report exit code")
}

func stopInnerDaemon(innerSock string) error {
	req, err := http.NewRequest(http.MethodPost, "http://localhost/stop", nil)
	if err != nil {
		return err
	}
	resp, err := innerHTTPClient(innerSock).Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode/100 != 2 {
		data, _ := io.ReadAll(resp.Body)
		return fmt.Errorf("inner stop failed: %s", strings.TrimSpace(string(data)))
	}
	return nil
}
