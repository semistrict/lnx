//go:build linux

package main

import (
	"fmt"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
)

const (
	unitDirs   = "/usr/lib/systemd/system:/etc/systemd/system:/lib/systemd/system"
	runDir     = "/run/lnx-services"
	enabledDir = "/etc/lnx-services/enabled"
	logDir     = "/var/log/lnx-services"
)

// installSystemctlShim copies the init binary to /usr/local/bin/lnx-init
// and creates a symlink at /usr/local/bin/systemctl.
// When invoked as "systemctl", main() dispatches to runSystemctl().
func installSystemctlShim() {
	initBin, err := os.ReadFile("/proc/self/exe")
	if err != nil {
		slog.Warn("failed to read init binary for systemctl shim", "error", err)
		return
	}
	os.MkdirAll("/usr/local/bin", 0755)
	if err := os.WriteFile("/usr/local/bin/lnx-init", initBin, 0755); err != nil {
		slog.Warn("failed to install lnx-init", "error", err)
		return
	}
	os.Remove("/usr/local/bin/systemctl")
	os.Symlink("/usr/local/bin/lnx-init", "/usr/local/bin/systemctl")
	// Some packages look in /usr/bin
	os.Remove("/usr/bin/systemctl")
	os.Symlink("/usr/local/bin/lnx-init", "/usr/bin/systemctl")

	os.MkdirAll(runDir, 0755)
	os.MkdirAll(enabledDir, 0755)
	os.MkdirAll(logDir, 0755)
}

// runSystemctl implements a minimal systemctl that parses systemd unit files.
func runSystemctl(args []string) int {
	// Strip flags that systemd clients pass.
	var cmd string
	var units []string
	for _, arg := range args {
		if strings.HasPrefix(arg, "-") {
			continue
		}
		if cmd == "" {
			cmd = arg
		} else {
			units = append(units, arg)
		}
	}

	switch cmd {
	case "start":
		for _, u := range units {
			if err := svcStart(u); err != nil {
				fmt.Fprintf(os.Stderr, "Failed to start %s: %v\n", u, err)
				return 1
			}
		}
	case "stop":
		for _, u := range units {
			svcStop(u)
		}
	case "restart":
		for _, u := range units {
			svcStop(u)
			if err := svcStart(u); err != nil {
				fmt.Fprintf(os.Stderr, "Failed to start %s: %v\n", u, err)
				return 1
			}
		}
	case "status":
		for _, u := range units {
			svcStatus(u)
		}
	case "enable":
		for _, u := range units {
			svcEnable(u)
		}
	case "disable":
		for _, u := range units {
			svcDisable(u)
		}
	case "is-active":
		for _, u := range units {
			if !svcIsActive(u) {
				fmt.Println("inactive")
				return 1
			}
			fmt.Println("active")
		}
	case "is-enabled":
		for _, u := range units {
			name := canonicalName(u)
			if _, err := os.Lstat(filepath.Join(enabledDir, name)); err == nil {
				fmt.Println("enabled")
			} else {
				fmt.Println("disabled")
				return 1
			}
		}
	case "daemon-reload", "show", "list-units", "cat", "mask", "unmask":
		// no-ops
	default:
		if cmd != "" {
			fmt.Fprintf(os.Stderr, "lnx-systemctl: unsupported command: %s\n", cmd)
			return 1
		}
	}
	return 0
}

func canonicalName(name string) string {
	if !strings.Contains(name, ".") {
		return name + ".service"
	}
	return name
}

func findUnit(name string) string {
	name = canonicalName(name)
	for _, dir := range strings.Split(unitDirs, ":") {
		path := filepath.Join(dir, name)
		if _, err := os.Stat(path); err == nil {
			return path
		}
	}
	return ""
}

func parseField(path, field string) string {
	data, err := os.ReadFile(path)
	if err != nil {
		return ""
	}
	for _, line := range strings.Split(string(data), "\n") {
		line = strings.TrimSpace(line)
		if strings.HasPrefix(line, field+"=") {
			return strings.TrimPrefix(line, field+"=")
		}
	}
	return ""
}

func pidFile(name string) string {
	return filepath.Join(runDir, strings.TrimSuffix(canonicalName(name), ".service")+".pid")
}

func isRunning(name string) bool {
	data, err := os.ReadFile(pidFile(name))
	if err != nil {
		return false
	}
	pid := atoi(strings.TrimSpace(string(data)))
	if pid <= 0 {
		return false
	}
	return syscall.Kill(pid, 0) == nil
}

func svcStart(name string) error {
	if isRunning(name) {
		return nil
	}

	unit := findUnit(name)
	if unit == "" {
		return fmt.Errorf("unit %s not found", name)
	}

	// Start dependencies.
	for _, field := range []string{"Requires", "Wants"} {
		deps := parseField(unit, field)
		for _, dep := range strings.Fields(deps) {
			if strings.HasSuffix(dep, ".target") || strings.HasSuffix(dep, ".socket") ||
				strings.Contains(dep, "network") {
				continue
			}
			if !isRunning(dep) {
				svcStart(dep)
			}
		}
	}

	execStart := parseField(unit, "ExecStart")
	if execStart == "" {
		return nil // oneshot with no ExecStart, or a target
	}
	execStart = strings.TrimPrefix(execStart, "-")

	// Docker uses -H fd:// for systemd socket activation; use unix socket instead.
	execStart = strings.ReplaceAll(execStart, "-H fd://", "-H unix:///var/run/docker.sock")

	// Run ExecStartPre if present.
	if pre := parseField(unit, "ExecStartPre"); pre != "" {
		pre = strings.TrimPrefix(pre, "-")
		preCmd := exec.Command("sh", "-c", pre)
		preCmd.Run()
	}

	svcName := strings.TrimSuffix(canonicalName(name), ".service")
	logPath := filepath.Join(logDir, svcName+".log")
	logFile, err := os.OpenFile(logPath, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0644)
	if err != nil {
		return fmt.Errorf("open log: %w", err)
	}

	cmd := exec.Command("sh", "-c", execStart)
	cmd.Stdout = logFile
	cmd.Stderr = logFile
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}

	if err := cmd.Start(); err != nil {
		logFile.Close()
		return fmt.Errorf("start: %w", err)
	}
	logFile.Close()

	os.WriteFile(pidFile(name), []byte(fmt.Sprintf("%d", cmd.Process.Pid)), 0644)
	go cmd.Wait() // reap

	return nil
}

func svcStop(name string) {
	unit := findUnit(name)
	if unit != "" {
		if execStop := parseField(unit, "ExecStop"); execStop != "" {
			execStop = strings.TrimPrefix(execStop, "-")
			exec.Command("sh", "-c", execStop).Run()
		}
	}

	data, err := os.ReadFile(pidFile(name))
	if err != nil {
		return
	}
	pid := atoi(strings.TrimSpace(string(data)))
	if pid > 0 {
		syscall.Kill(pid, syscall.SIGTERM)
		// Brief wait for clean shutdown.
		for i := 0; i < 10; i++ {
			if syscall.Kill(pid, 0) != nil {
				break
			}
			// Can't import time in a simple way here, use a busy loop with nanosleep
			var ts syscall.Timespec
			ts.Sec = 0
			ts.Nsec = 200_000_000
			syscall.Nanosleep(&ts, nil)
		}
		syscall.Kill(pid, syscall.SIGKILL)
	}
	os.Remove(pidFile(name))
}

func svcStatus(name string) {
	svcName := strings.TrimSuffix(canonicalName(name), ".service")
	if isRunning(name) {
		data, _ := os.ReadFile(pidFile(name))
		fmt.Printf("● %s.service - active (running)\n", svcName)
		fmt.Printf("  PID: %s\n", strings.TrimSpace(string(data)))
	} else {
		fmt.Printf("● %s.service - inactive (dead)\n", svcName)
	}
}

func svcIsActive(name string) bool {
	return isRunning(name)
}

func svcEnable(name string) {
	unit := findUnit(name)
	if unit == "" {
		fmt.Fprintf(os.Stderr, "Unit %s not found\n", name)
		return
	}
	svcName := strings.TrimSuffix(canonicalName(name), ".service")
	os.MkdirAll(enabledDir, 0755)
	os.Symlink(unit, filepath.Join(enabledDir, svcName))
}

func svcDisable(name string) {
	svcName := strings.TrimSuffix(canonicalName(name), ".service")
	os.Remove(filepath.Join(enabledDir, svcName))
}

// startEnabledServices starts all services that were enabled via `systemctl enable`.
func startEnabledServices() {
	entries, err := os.ReadDir(enabledDir)
	if err != nil {
		return
	}
	for _, e := range entries {
		name := e.Name()
		if err := svcStart(name); err != nil {
			slog.Warn("failed to start enabled service", "name", name, "error", err)
		} else {
			slog.Info("started enabled service", "name", name)
		}
	}
}

func atoi(s string) int {
	n := 0
	for _, c := range s {
		if c >= '0' && c <= '9' {
			n = n*10 + int(c-'0')
		}
	}
	return n
}
