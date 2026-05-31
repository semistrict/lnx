//go:build linux

package main

import (
	"fmt"
	"log/slog"
	"os"
	"os/exec"
)

// buildDaemonCmd creates the command to spawn the daemon process.
// On Linux, the daemon needs root for TAP/KVM/block devices.
// If not already root, wraps with sudo and passes through env vars.
func buildDaemonCmd(self string, daemonArgs []string) *exec.Cmd {
	if os.Getuid() == 0 {
		return exec.Command(self, daemonArgs...)
	}

	// Check if linux_host experiment is enabled — if not, no sudo needed
	// (buildVM will return an error anyway).
	if os.Getenv("LNX_EXPERIMENTS") == "" {
		return exec.Command(self, daemonArgs...)
	}

	// The daemon needs root for TAP/KVM/block devices.
	// Use sudo with explicit env vars since sudo resets the environment.
	envArgs := []string{
		fmt.Sprintf("HOME=%s", os.Getenv("HOME")),
	}
	if parent := os.Getenv("LNX_PARENT"); parent != "" {
		envArgs = append(envArgs, fmt.Sprintf("LNX_PARENT=%s", parent))
	}
	if logLevel := os.Getenv("LNX_LOG"); logLevel != "" {
		envArgs = append(envArgs, fmt.Sprintf("LNX_LOG=%s", logLevel))
	}
	if experiments := os.Getenv("LNX_EXPERIMENTS"); experiments != "" {
		envArgs = append(envArgs, fmt.Sprintf("LNX_EXPERIMENTS=%s", experiments))
	}

	// sudo VAR=val command args...
	args := append(envArgs, self)
	args = append(args, daemonArgs...)
	cmd := exec.Command("sudo", args...)

	// Log the command for debugging.
	slog.Debug("spawning daemon with sudo", "args", cmd.Args)
	return cmd
}
