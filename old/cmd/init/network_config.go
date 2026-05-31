//go:build linux

package main

import (
	"fmt"
	"net"
	"strings"

	"github.com/insomniacslk/dhcp/dhcpv4"
)

func cidrForLease(ip net.IP, mask net.IPMask) (string, error) {
	ip = ip.To4()
	if ip == nil {
		return "", fmt.Errorf("missing ipv4 address")
	}
	if len(mask) != net.IPv4len {
		return "", fmt.Errorf("missing subnet mask")
	}
	ones, bits := mask.Size()
	if bits != 32 {
		return "", fmt.Errorf("invalid subnet mask")
	}
	return fmt.Sprintf("%s/%d", ip.String(), ones), nil
}

func resolvConfForLease(ack *dhcpv4.DHCPv4) string {
	if ack == nil {
		return ""
	}

	var lines []string
	if search := ack.DomainSearch(); search != nil && len(search.Labels) > 0 {
		lines = append(lines, "search "+strings.Join(search.Labels, " "))
	} else if domain := strings.TrimSpace(ack.DomainName()); domain != "" {
		lines = append(lines, "search "+domain)
	}
	for _, dns := range ack.DNS() {
		if ip := dns.To4(); ip != nil {
			lines = append(lines, "nameserver "+ip.String())
		}
	}
	if len(lines) == 0 {
		return ""
	}
	return strings.Join(lines, "\n") + "\n"
}
