package lnx

import (
	"io"
	"log/slog"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"time"
)

// guiDeps maps brew package names to the binary names they install.
var guiDeps = map[string]string{
	"cocoa-way":      "cocoa-way",
	"waypipe-darwin": "waypipe", // brew package is waypipe-darwin, binary is waypipe
}

// MissingGUIDeps returns the brew package names of GUI dependencies not found in PATH.
func MissingGUIDeps() []string {
	var missing []string
	for pkg, bin := range guiDeps {
		if _, err := exec.LookPath(bin); err != nil {
			missing = append(missing, pkg)
		}
	}
	return missing
}

// guiState holds the running GUI processes on the host side.
type guiState struct {
	cocoaWay *exec.Cmd
	waypipe  *exec.Cmd
	listener net.Listener // vsock listener for waypipe port
	tmpDir   string       // temp dir used as XDG_RUNTIME_DIR and waypipe socket dir
}

func (g *guiState) close() {
	if g.waypipe != nil && g.waypipe.Process != nil {
		g.waypipe.Process.Kill()
		g.waypipe.Wait()
	}
	if g.cocoaWay != nil && g.cocoaWay.Process != nil {
		g.cocoaWay.Process.Kill()
		g.cocoaWay.Wait()
	}
	if g.listener != nil {
		g.listener.Close()
	}
	if g.tmpDir != "" {
		os.RemoveAll(g.tmpDir)
	}
}

// run accepts the vsock connection from the guest's waypipe server first
// (unblocking the guest), then starts cocoa-way and waypipe client on the host,
// and relays traffic between them.
func (g *guiState) run() {
	// 1. Accept vsock connection FIRST so the guest's waypipe server can
	// complete its connect() and create the Wayland display.
	vsockConn, err := g.listener.Accept()
	if err != nil {
		slog.Error("gui: vsock accept failed", "error", err)
		return
	}
	slog.Info("gui: guest waypipe connected")

	// cocoa-way ignores XDG_RUNTIME_DIR and always creates its socket at
	// $TMPDIR/cocoa-way/wayland-1. Use that path.
	cocoaWayRuntimeDir := filepath.Join(os.TempDir(), "cocoa-way")

	// Create our own temp dir for waypipe client socket.
	g.tmpDir, err = os.MkdirTemp("", "lnx-gui-*")
	if err != nil {
		slog.Error("gui: create temp dir", "error", err)
		vsockConn.Close()
		return
	}

	// 2. Start cocoa-way compositor.
	home, _ := os.UserHomeDir()
	cocoaWayBin, _ := exec.LookPath("cocoa-way")
	g.cocoaWay = exec.Command(cocoaWayBin)
	cwLog, err := os.Create(filepath.Join(home, ".lnx", "cocoa-way.log"))
	if err == nil {
		g.cocoaWay.Stdout = cwLog
		g.cocoaWay.Stderr = cwLog
	}
	if err := g.cocoaWay.Start(); err != nil {
		slog.Error("gui: start cocoa-way", "error", err)
		vsockConn.Close()
		return
	}
	slog.Info("gui: cocoa-way started", "pid", g.cocoaWay.Process.Pid)

	// Wait for cocoa-way to create its Wayland socket.
	waylandSock := filepath.Join(cocoaWayRuntimeDir, "wayland-1")
	if !waitForFile(waylandSock, 10*time.Second) {
		slog.Error("gui: cocoa-way did not create Wayland socket", "expected", waylandSock)
		vsockConn.Close()
		return
	}
	slog.Info("gui: cocoa-way socket ready", "path", waylandSock)

	// 3. Start waypipe client. It creates a unix socket and listens for server connections.
	wpSocketPath := filepath.Join(g.tmpDir, "waypipe-client.sock")
	waypipeBin, _ := exec.LookPath("waypipe")
	g.waypipe = exec.Command(waypipeBin, "-d", "-s", wpSocketPath, "client")
	g.waypipe.Env = append(os.Environ(),
		"XDG_RUNTIME_DIR="+cocoaWayRuntimeDir,
		"WAYLAND_DISPLAY=wayland-1",
	)
	wpLog, err := os.Create(filepath.Join(home, ".lnx", "waypipe-client.log"))
	if err == nil {
		g.waypipe.Stdout = wpLog
		g.waypipe.Stderr = wpLog
	}
	if err := g.waypipe.Start(); err != nil {
		slog.Error("gui: start waypipe client", "error", err)
		vsockConn.Close()
		return
	}
	slog.Info("gui: waypipe client started", "pid", g.waypipe.Process.Pid)

	// Wait for waypipe client to create its socket.
	if !waitForFile(wpSocketPath, 5*time.Second) {
		slog.Error("gui: waypipe client did not create socket", "path", wpSocketPath)
		vsockConn.Close()
		return
	}

	// 4. Connect to waypipe client's unix socket.
	wpConn, err := net.Dial("unix", wpSocketPath)
	if err != nil {
		slog.Error("gui: connect to waypipe client socket", "error", err)
		vsockConn.Close()
		return
	}

	slog.Info("gui: relay active, forwarding Wayland protocol over vsock")

	// 5. Bidirectional relay between vsock (guest) and unix (host waypipe client).
	go func() {
		done := make(chan struct{}, 2)
		go func() {
			io.Copy(wpConn, vsockConn)
			done <- struct{}{}
		}()
		go func() {
			io.Copy(vsockConn, wpConn)
			done <- struct{}{}
		}()
		<-done
		vsockConn.Close()
		wpConn.Close()
		<-done
		slog.Info("gui: relay closed")
	}()
}

func waitForFile(path string, timeout time.Duration) bool {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		if _, err := os.Stat(path); err == nil {
			return true
		}
		time.Sleep(50 * time.Millisecond)
	}
	return false
}
