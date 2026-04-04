//go:build linux

package main

import (
	"fmt"
	"log/slog"
	"math/rand/v2"
	"os"
	"os/exec"
)

func configureNetwork() {
	runCmd("/sbin/ip", "link", "set", "lo", "up")

	iface := findNetInterface()
	if iface == "" {
		slog.Warn("no network interface found")
		return
	}

	runCmd("/sbin/ip", "link", "set", iface, "up")

	// VZ framework uses 192.168.64.0/24 for NAT with gateway at .1.
	// Use static config with a random IP to avoid collisions when
	// multiple VMs run concurrently.
	ip := fmt.Sprintf("192.168.64.%d", 2+rand.IntN(253))
	runCmd("/sbin/ip", "addr", "add", ip+"/24", "dev", iface)
	runCmd("/sbin/ip", "route", "add", "default", "via", "192.168.64.1", "dev", iface)

	slog.Info("network configured", "iface", iface)
}

func writeResolvConf() {
	// Remove the systemd symlink if present and write a real file.
	os.Remove("/etc/resolv.conf")
	if err := os.WriteFile("/etc/resolv.conf", []byte("nameserver 8.8.8.8\nnameserver 8.8.4.4\n"), 0644); err != nil {
		slog.Warn("failed to write resolv.conf", "error", err)
	}
}

func findNetInterface() string {
	entries, err := os.ReadDir("/sys/class/net")
	if err != nil {
		return ""
	}
	for _, e := range entries {
		name := e.Name()
		if name == "lo" {
			continue
		}
		if _, err := os.Stat("/sys/class/net/" + name + "/device"); err == nil {
			return name
		}
	}
	return ""
}

func runCmd(name string, args ...string) {
	if out, err := exec.Command(name, args...).CombinedOutput(); err != nil {
		slog.Warn("command failed", "cmd", append([]string{name}, args...), "error", err, "output", string(out))
	}
}
