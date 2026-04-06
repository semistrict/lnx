//go:build linux

package main

import (
	"fmt"
	"log/slog"
	"net"
	"os"
	"os/exec"
	"strings"
	"syscall"
	"time"
)

// startGUI starts a headless Wayland compositor + VNC server + noVNC websocket proxy.
// The desktop is accessible via browser at http://localhost:6080/vnc.html
func startGUI() {
	os.MkdirAll("/run/lnx-gui", 0700)
	os.MkdirAll("/run/dbus", 0755)
	if setupUID > 0 {
		_ = os.Chown("/run/lnx-gui", setupUID, setupUID)
	}

	// Start system D-Bus (needed by desktop services).
	if dbusPath, err := exec.LookPath("dbus-daemon"); err == nil {
		sysBus := exec.Command(dbusPath, "--system", "--nofork", "--nopidfile")
		sysBus.Stdout = os.Stderr
		sysBus.Stderr = os.Stderr
		if err := sysBus.Start(); err != nil {
			slog.Warn("gui: system dbus failed", "error", err)
		} else {
			go sysBus.Wait()
			slog.Info("gui: system dbus started", "pid", sysBus.Process.Pid)
		}
	}

	// Start session D-Bus.
	if dbusPath, err := exec.LookPath("dbus-daemon"); err == nil {
		dbus := exec.Command(dbusPath, "--session", "--address=unix:path=/run/lnx-gui/bus", "--nofork", "--nopidfile")
		dbus.Env = append(desktopEnv(), "DBUS_VERBOSE=0")
		applyUserCredential(dbus)
		dbus.Stdout = os.Stderr
		dbus.Stderr = os.Stderr
		if err := dbus.Start(); err != nil {
			slog.Warn("gui: session dbus failed", "error", err)
		} else {
			go dbus.Wait()
			os.Setenv("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/lnx-gui/bus")
			slog.Info("gui: session dbus started", "pid", dbus.Process.Pid)
		}
	}

	compositor, args := findCompositor()
	if compositor == "" {
		slog.Warn("gui: no compositor found (install labwc or cage)")
		return
	}

	// Build env without WAYLAND_DISPLAY/DISPLAY (compositor creates its own).
	env := desktopEnv()
	env = append(env,
		"WLR_BACKENDS=headless",
		"WLR_LIBINPUT_NO_DEVICES=1",
		"WLR_RENDERER=pixman",
	)

	cmd := exec.Command(compositor, args...)
	cmd.Env = env
	applyUserCredential(cmd)
	cmd.Stdout = os.Stderr
	cmd.Stderr = os.Stderr
	if err := cmd.Start(); err != nil {
		slog.Error("gui: compositor failed", "error", err)
		return
	}
	go cmd.Wait()
	slog.Info("gui: compositor started", "cmd", compositor, "pid", cmd.Process.Pid)

	// Wait for Wayland socket.
	wayland := waitForWaylandSocket("/run/lnx-gui")
	if wayland == "" {
		slog.Error("gui: compositor did not create Wayland socket")
		return
	}
	slog.Info("gui: Wayland socket ready", "display", wayland)

	// Start wayvnc (plain VNC on port 5900).
	wayvncPath, err := exec.LookPath("wayvnc")
	if err != nil {
		slog.Warn("gui: wayvnc not found")
		return
	}
	vnc := exec.Command(wayvncPath, "127.0.0.1", "5900")
	vnc.Env = append(env, "WAYLAND_DISPLAY="+wayland)
	applyUserCredential(vnc)
	vnc.Stdout = os.Stderr
	vnc.Stderr = os.Stderr
	if err := vnc.Start(); err != nil {
		slog.Error("gui: wayvnc failed", "error", err)
		return
	}
	go vnc.Wait()
	slog.Info("gui: wayvnc started", "pid", vnc.Process.Pid, "port", 5900)
	go warnIfPortNotReady("wayvnc", 5900, 10*time.Second)

	// Start websockify to bridge VNC→WebSocket and serve noVNC HTML on port 6080.
	wsPath := "/usr/share/novnc/utils/novnc_proxy"
	if _, err := os.Stat(wsPath); err != nil {
		wsPath2, err2 := exec.LookPath("websockify")
		if err2 != nil {
			slog.Warn("gui: neither novnc_proxy nor websockify found")
			return
		}
		ws := exec.Command(wsPath2, "6080", "localhost:5900", "--web", "/usr/share/novnc")
		ws.Stdout = os.Stderr
		ws.Stderr = os.Stderr
		if err := ws.Start(); err != nil {
			slog.Error("gui: websockify failed", "error", err)
			return
		}
		go ws.Wait()
		slog.Info("gui: websockify started", "pid", ws.Process.Pid)
	} else {
		ws := exec.Command(wsPath, "--vnc", "localhost:5900", "--listen", "6080")
		ws.Stdout = os.Stderr
		ws.Stderr = os.Stderr
		if err := ws.Start(); err != nil {
			slog.Error("gui: novnc_proxy failed", "error", err)
			return
		}
		go ws.Wait()
		slog.Info("gui: novnc_proxy started", "pid", ws.Process.Pid)
	}
	go warnIfPortNotReady("noVNC", 6080, 10*time.Second)

	// Set env so exec sessions can launch GUI apps.
	os.Setenv("XDG_RUNTIME_DIR", "/run/lnx-gui")
	os.Setenv("WAYLAND_DISPLAY", wayland)
	os.Setenv("DISPLAY", ":0")
	os.Setenv("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/lnx-gui/bus")
	os.Setenv("GTK_THEME", "Yaru")
	os.Setenv("DESKTOP_SESSION", "xfce")
	os.Setenv("XDG_SESSION_DESKTOP", "xfce")
	os.Setenv("XDG_CURRENT_DESKTOP", "XFCE")
	os.Setenv("XCURSOR_THEME", "Yaru")

	slog.Info("gui: desktop ready on port 6080")
}

func desktopEnv() []string {
	var env []string
	for _, kv := range os.Environ() {
		k, _, _ := strings.Cut(kv, "=")
		if k == "WAYLAND_DISPLAY" || k == "DISPLAY" {
			continue
		}
		env = append(env, kv)
	}
	env = append(env,
		"XDG_RUNTIME_DIR=/run/lnx-gui",
		"DBUS_SESSION_BUS_ADDRESS=unix:path=/run/lnx-gui/bus",
		"DESKTOP_SESSION=xfce",
		"XDG_SESSION_DESKTOP=xfce",
		"XDG_CURRENT_DESKTOP=XFCE",
		"GTK_THEME=Yaru",
		"XCURSOR_THEME=Yaru",
		"XCURSOR_SIZE=24",
	)
	return env
}

func applyUserCredential(cmd *exec.Cmd) {
	if setupUID <= 0 {
		return
	}
	cmd.SysProcAttr = &syscall.SysProcAttr{
		Credential: &syscall.Credential{
			Uid:    uint32(setupUID),
			Gid:    uint32(setupUID),
			Groups: lookupSupplementaryGroups(setupUID),
		},
	}
}

func findCompositor() (string, []string) {
	if path, err := exec.LookPath("labwc"); err == nil {
		var app string
		if _, err := exec.LookPath("startxfce4"); err == nil {
			app = "startxfce4"
		} else if _, err := exec.LookPath("foot"); err == nil {
			app = "foot --font=monospace:size=14"
		} else {
			app = "bash"
		}
		return path, []string{"-s", app}
	}
	if path, err := exec.LookPath("cage"); err == nil {
		if footPath, err := exec.LookPath("foot"); err == nil {
			return path, []string{"-d", "--", footPath, "--font=monospace:size=14"}
		}
		return path, []string{"-d", "--", "bash"}
	}
	return "", nil
}

func waitForWaylandSocket(dir string) string {
	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		entries, _ := os.ReadDir(dir)
		for _, e := range entries {
			if strings.HasPrefix(e.Name(), "wayland-") && !strings.HasSuffix(e.Name(), ".lock") {
				return e.Name()
			}
		}
		time.Sleep(50 * time.Millisecond)
	}
	return ""
}

func waitForPort(port int, timeout time.Duration) bool {
	addr := fmt.Sprintf("127.0.0.1:%d", port)
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		conn, err := net.DialTimeout("tcp", addr, 200*time.Millisecond)
		if err == nil {
			conn.Close()
			return true
		}
		time.Sleep(100 * time.Millisecond)
	}
	return false
}

func warnIfPortNotReady(name string, port int, timeout time.Duration) {
	if !waitForPort(port, timeout) {
		slog.Warn("gui: port not ready", "service", name, "port", port)
	}
}
