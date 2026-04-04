package lnxnet

import (
	"net"
	"testing"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestParseEthernet(t *testing.T) {
	dst := net.HardwareAddr{0x01, 0x02, 0x03, 0x04, 0x05, 0x06}
	src := net.HardwareAddr{0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f}
	payload := []byte("hello")

	frame := buildEthernet(dst, src, etherTypeIPv4, payload)

	eth := parseEthernet(frame)
	require.NotNil(t, eth)
	assert.Equal(t, dst, net.HardwareAddr(eth.DstMAC))
	assert.Equal(t, src, net.HardwareAddr(eth.SrcMAC))
	assert.Equal(t, uint16(etherTypeIPv4), eth.EtherType)
	assert.Equal(t, payload, eth.Payload)
}

func TestParseEthernet_TooShort(t *testing.T) {
	assert.Nil(t, parseEthernet([]byte{1, 2, 3}))
}

func TestBuildEthernet_RoundTrip(t *testing.T) {
	dst := net.HardwareAddr{0xff, 0xff, 0xff, 0xff, 0xff, 0xff}
	src := net.HardwareAddr{0x02, 0x00, 0x00, 0x00, 0x00, 0x01}

	frame := buildEthernet(dst, src, etherTypeARP, []byte{42})
	eth := parseEthernet(frame)
	require.NotNil(t, eth)
	assert.Equal(t, uint16(etherTypeARP), eth.EtherType)
	assert.Equal(t, []byte{42}, eth.Payload)
}
