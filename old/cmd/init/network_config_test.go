//go:build linux

package main

import (
	"net"
	"testing"

	"github.com/insomniacslk/dhcp/dhcpv4"
	"github.com/insomniacslk/dhcp/rfc1035label"
	"github.com/stretchr/testify/require"
)

func TestCIDRForLease(t *testing.T) {
	cidr, err := cidrForLease(net.IPv4(10, 20, 30, 40), net.CIDRMask(20, 32))
	require.NoError(t, err)
	require.Equal(t, "10.20.30.40/20", cidr)
}

func TestResolvConfForLease(t *testing.T) {
	search := rfc1035label.NewLabels()
	search.Labels = []string{"corp.example", "example"}

	ack, err := dhcpv4.New(
		dhcpv4.WithDNS(net.IPv4(1, 1, 1, 1), net.IPv4(8, 8, 8, 8)),
		dhcpv4.WithOption(dhcpv4.OptDomainSearch(search)),
	)
	require.NoError(t, err)

	require.Equal(t, "search corp.example example\nnameserver 1.1.1.1\nnameserver 8.8.8.8\n", resolvConfForLease(ack))
}
