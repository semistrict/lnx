package lnxnet

import (
	"net"
	"testing"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestBuildIPv4_Checksum(t *testing.T) {
	src := net.ParseIP("192.168.64.1").To4()
	dst := net.ParseIP("192.168.64.2").To4()
	payload := []byte("test data")

	pkt := buildIPv4(src, dst, protoUDP, payload)
	require.True(t, len(pkt) >= 20)

	// Verify header checksum is valid by summing the header — should be 0.
	var sum uint32
	for i := 0; i < 20; i += 2 {
		sum += uint32(pkt[i])<<8 | uint32(pkt[i+1])
	}
	for sum > 0xffff {
		sum = (sum >> 16) + (sum & 0xffff)
	}
	assert.Equal(t, uint16(0xffff), uint16(sum))
}

func TestParseIPv4_RoundTrip(t *testing.T) {
	src := net.ParseIP("10.0.0.1").To4()
	dst := net.ParseIP("10.0.0.2").To4()
	payload := []byte("hello")

	pkt := buildIPv4(src, dst, protoTCP, payload)
	ip := parseIPv4(pkt)
	require.NotNil(t, ip)

	assert.Equal(t, src, ip.SrcIP.To4())
	assert.Equal(t, dst, ip.DstIP.To4())
	assert.Equal(t, uint8(protoTCP), ip.Protocol)
	assert.Equal(t, payload, ip.Payload)
}

func TestParseIPv4_TooShort(t *testing.T) {
	assert.Nil(t, parseIPv4([]byte{1, 2, 3}))
}
