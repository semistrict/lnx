//go:build linux

package main

import (
	"log/slog"
	"os"
	"os/exec"
	"strings"
	"time"
)

// startCompositor starts seatd and a Wayland compositor on the virtio-gpu display.
func startCompositor() {
	if _, err := os.Stat("/dev/dri/card0"); err != nil {
		slog.Warn("gui: no DRM device found, skipping compositor")
		return
	}

	compositor, args := findCompositor()
	if compositor == "" {
		slog.Warn("gui: no compositor found (install cage or labwc)")
		return
	}

	os.MkdirAll("/run/lnx-gui", 0777)
	os.Chmod("/run/lnx-gui", 0777)

	// Fix device permissions.
	for _, dev := range []string{"/dev/tty0", "/dev/tty1", "/dev/tty2",
		"/dev/dri/card0", "/dev/dri/renderD128"} {
		os.Chmod(dev, 0666)
	}

	// Unbind fbcon so the DRM scanout is available for the compositor.
	// Without this, VZ framework shows the fbcon buffer instead of KMS output.
	os.WriteFile("/sys/class/vtconsole/vtcon1/bind", []byte("0"), 0644)

	// Start seatd daemon for seat management.
	if seatdPath, err := exec.LookPath("seatd"); err == nil {
		seatd := exec.Command(seatdPath, "-g", "seat")
		seatd.Stdout = os.Stderr
		seatd.Stderr = os.Stderr
		if err := seatd.Start(); err != nil {
			slog.Warn("gui: failed to start seatd", "error", err)
		} else {
			slog.Info("gui: seatd started", "pid", seatd.Process.Pid)
			// Wait for seatd socket.
			for i := 0; i < 20; i++ {
				if _, err := os.Stat("/run/seatd.sock"); err == nil {
					break
				}
				time.Sleep(50 * time.Millisecond)
			}
		}
	}

	// Build clean env for cage — strip WAYLAND_DISPLAY/DISPLAY.
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
		"WLR_BACKENDS=drm",
		"WLR_DRM_DEVICES=/dev/dri/card0",
		"LIBSEAT_BACKEND=seatd",
	)

	cmd := exec.Command(compositor, args...)
	cmd.Env = env
	cmd.Stdout = os.Stderr
	cmd.Stderr = os.Stderr
	if err := cmd.Start(); err != nil {
		slog.Error("gui: failed to start compositor", "compositor", compositor, "error", err)
		return
	}
	slog.Info("gui: compositor started", "compositor", compositor, "pid", cmd.Process.Pid)

	os.Setenv("XDG_RUNTIME_DIR", "/run/lnx-gui")
	os.Setenv("WAYLAND_DISPLAY", "wayland-1")
}

func findCompositor() (string, []string) {
	if path, err := exec.LookPath("cage"); err == nil {
		if footPath, err := exec.LookPath("foot"); err == nil {
			return path, []string{"-d", "--", footPath, "--font=monospace:size=14"}
		}
		return path, []string{"-d", "--", "bash"}
	}
	if path, err := exec.LookPath("labwc"); err == nil {
		return path, []string{"-s", "foot"}
	}
	return "", nil
}
