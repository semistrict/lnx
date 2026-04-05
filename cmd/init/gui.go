//go:build linux

package main

import (
	"fmt"
	"log/slog"
	"os"
	"os/exec"
	"syscall"

	"github.com/semistrict/lnx/internal/protocol"
)

// startGUIForwarding starts waypipe server in the guest using vsock to connect
// directly to the host's waypipe client.
func startGUIForwarding() {
	waypipePath, err := exec.LookPath("waypipe")
	if err != nil {
		slog.Warn("waypipe not found in guest, GUI forwarding disabled")
		return
	}
	slog.Info("gui: starting waypipe server", "path", waypipePath)

	os.MkdirAll("/run/lnx-gui", 0777)
	os.Chmod("/run/lnx-gui", 0777)

	// Set permissive umask so waypipe creates the Wayland socket world-accessible.
	// The exec user (non-root) needs to connect to it.
	oldUmask := syscall.Umask(0)

	// waypipe server connects TO the client (on the host) via vsock.
	// -s 2:<port> means connect to host CID 2 on the waypipe port.
	// --display sets the Wayland socket name for guest apps.
	socketArg := fmt.Sprintf("2:%d", protocol.WaypipePort)
	cmd := exec.Command(waypipePath,
		"--vsock",
		"-s", socketArg,
		"--display", "wayland-1",
		"server", "--",
		"sleep", "infinity",
	)
	cmd.Env = append(os.Environ(), "XDG_RUNTIME_DIR=/run/lnx-gui")
	cmd.Stdout = os.Stderr
	cmd.Stderr = os.Stderr
	if err := cmd.Start(); err != nil {
		slog.Error("gui: failed to start waypipe server", "error", err)
		return
	}
	slog.Info("gui: waypipe server started", "pid", cmd.Process.Pid)
	syscall.Umask(oldUmask)

	// Set environment so all guest processes can use the Wayland display.
	os.Setenv("XDG_RUNTIME_DIR", "/run/lnx-gui")
	os.Setenv("WAYLAND_DISPLAY", "wayland-1")
}
