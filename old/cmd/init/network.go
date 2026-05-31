//go:build linux

package main

import (
	"context"
	"fmt"
	"log/slog"
	"net"
	"os"
	"os/exec"
	"time"

	"github.com/insomniacslk/dhcp/dhcpv4"
	"github.com/insomniacslk/dhcp/dhcpv4/nclient4"
)

func configureNetwork() {
	runCmd("/sbin/ip", "link", "set", "lo", "up")

	iface := findNetInterface()
	if iface == "" {
		slog.Warn("no network interface found")
		return
	}

	runCmd("/sbin/ip", "link", "set", iface, "up")

	lease, err := requestDHCPLease(iface)
	if err != nil {
		slog.Warn("dhcp lease failed", "iface", iface, "error", err)
		return
	}
	if err := applyDHCPLease(iface, lease.ACK); err != nil {
		slog.Warn("apply dhcp lease failed", "iface", iface, "error", err)
		return
	}
	if err := writeResolvConf(lease.ACK); err != nil {
		slog.Warn("failed to write resolv.conf", "error", err)
	}

	slog.Info("network configured", "iface", iface, "ip", lease.ACK.YourIPAddr, "router", lease.ACK.Router(), "dns", lease.ACK.DNS())
}

func requestDHCPLease(iface string) (*nclient4.Lease, error) {
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()

	client, err := nclient4.New(iface,
		nclient4.WithRetry(3),
		nclient4.WithTimeout(4*time.Second),
	)
	if err != nil {
		return nil, fmt.Errorf("create dhcp client: %w", err)
	}
	defer client.Close()

	lease, err := client.Request(ctx, dhcpv4.WithRequestedOptions(
		dhcpv4.OptionSubnetMask,
		dhcpv4.OptionRouter,
		dhcpv4.OptionDomainNameServer,
		dhcpv4.OptionDNSDomainSearchList,
		dhcpv4.OptionDomainName,
	))
	if err != nil {
		return nil, fmt.Errorf("request dhcp lease: %w", err)
	}
	if lease == nil || lease.ACK == nil {
		return nil, fmt.Errorf("missing dhcp ack")
	}
	return lease, nil
}

func applyDHCPLease(iface string, ack *dhcpv4.DHCPv4) error {
	if ack == nil {
		return fmt.Errorf("missing dhcp ack")
	}

	cidr, err := cidrForLease(ack.YourIPAddr, ack.SubnetMask())
	if err != nil {
		return err
	}

	runCmd("/sbin/ip", "addr", "flush", "dev", iface)
	runCmd("/sbin/ip", "addr", "add", cidr, "dev", iface)
	runCmd("/sbin/ip", "route", "del", "default")

	routers := ack.Router()
	if len(routers) > 0 {
		gateway := firstIPv4(routers)
		if gateway != nil {
			runCmd("/sbin/ip", "route", "replace", "default", "via", gateway.String(), "dev", iface)
		}
	}

	return nil
}

func writeResolvConf(ack *dhcpv4.DHCPv4) error {
	// Remove the systemd symlink if present and write a real file.
	os.Remove("/etc/resolv.conf")
	data := resolvConfForLease(ack)
	if data == "" {
		return nil
	}
	if err := os.WriteFile("/etc/resolv.conf", []byte(data), 0644); err != nil {
		return err
	}
	return nil
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

func firstIPv4(ips []net.IP) net.IP {
	for _, ip := range ips {
		if v4 := ip.To4(); v4 != nil {
			return v4
		}
	}
	return nil
}

func runCmd(name string, args ...string) {
	if out, err := exec.Command(name, args...).CombinedOutput(); err != nil {
		slog.Warn("command failed", "cmd", append([]string{name}, args...), "error", err, "output", string(out))
	}
}
