//go:build linux

package lnx

import (
	"fmt"
	"log/slog"
	"os"
	"os/exec"
)

const (
	tapDevice  = "lnxtap0"
	tapIP      = "192.168.64.1"
	tapSubnet  = "192.168.64.0/24"
	tapCIDR    = tapIP + "/24"
)

// setupTAP creates a TAP device for Firecracker networking and configures
// NAT so the guest can reach the internet. Requires root/CAP_NET_ADMIN.
// Idempotent: if the TAP already exists, reconfigures it.
func setupTAP() error {
	// Delete stale TAP if it exists, then recreate.
	exec.Command("ip", "link", "set", tapDevice, "down").Run()
	exec.Command("ip", "tuntap", "del", "dev", tapDevice, "mode", "tap").Run()

	cmds := [][]string{
		{"ip", "tuntap", "add", "dev", tapDevice, "mode", "tap"},
		{"ip", "addr", "add", tapCIDR, "dev", tapDevice},
		{"ip", "link", "set", tapDevice, "up"},
	}
	for _, args := range cmds {
		if out, err := exec.Command(args[0], args[1:]...).CombinedOutput(); err != nil {
			return fmt.Errorf("%v: %s: %w", args, out, err)
		}
	}

	// Enable IP forwarding.
	if err := os.WriteFile("/proc/sys/net/ipv4/ip_forward", []byte("1"), 0644); err != nil {
		return fmt.Errorf("enable ip_forward: %w", err)
	}

	// Find the default route interface for MASQUERADE.
	outIface, err := defaultRouteInterface()
	if err != nil {
		return fmt.Errorf("find default route: %w", err)
	}

	// Set up NAT.
	natCmds := [][]string{
		{"iptables", "-t", "nat", "-A", "POSTROUTING", "-o", outIface, "-s", tapSubnet, "-j", "MASQUERADE"},
		{"iptables", "-A", "FORWARD", "-i", tapDevice, "-o", outIface, "-j", "ACCEPT"},
		{"iptables", "-A", "FORWARD", "-i", outIface, "-o", tapDevice, "-m", "state", "--state", "RELATED,ESTABLISHED", "-j", "ACCEPT"},
	}
	for _, args := range natCmds {
		if out, err := exec.Command(args[0], args[1:]...).CombinedOutput(); err != nil {
			return fmt.Errorf("%v: %s: %w", args, out, err)
		}
	}

	slog.Info("TAP network configured", "device", tapDevice, "ip", tapCIDR, "nat", outIface)
	return nil
}

// teardownTAP removes the TAP device and NAT rules.
func teardownTAP() {
	outIface, _ := defaultRouteInterface()

	// Best-effort cleanup — ignore errors.
	exec.Command("iptables", "-t", "nat", "-D", "POSTROUTING", "-o", outIface, "-s", tapSubnet, "-j", "MASQUERADE").Run()
	exec.Command("iptables", "-D", "FORWARD", "-i", tapDevice, "-o", outIface, "-j", "ACCEPT").Run()
	exec.Command("iptables", "-D", "FORWARD", "-i", outIface, "-o", tapDevice, "-m", "state", "--state", "RELATED,ESTABLISHED", "-j", "ACCEPT").Run()
	exec.Command("ip", "link", "del", tapDevice).Run()
}

// defaultRouteInterface returns the network interface used for the default route.
func defaultRouteInterface() (string, error) {
	out, err := exec.Command("ip", "route", "show", "default").Output()
	if err != nil {
		return "", err
	}
	// Format: "default via X.X.X.X dev <iface> ..."
	fields := splitFields(string(out))
	for i, f := range fields {
		if f == "dev" && i+1 < len(fields) {
			return fields[i+1], nil
		}
	}
	return "", fmt.Errorf("no default route found")
}

// splitFields splits on whitespace including newlines.
func splitFields(s string) []string {
	var fields []string
	start := -1
	for i, c := range s {
		if c == ' ' || c == '\t' || c == '\n' || c == '\r' {
			if start >= 0 {
				fields = append(fields, s[start:i])
				start = -1
			}
		} else if start < 0 {
			start = i
		}
	}
	if start >= 0 {
		fields = append(fields, s[start:])
	}
	return fields
}
