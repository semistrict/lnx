package lnxnet

import (
	"testing"

	"github.com/stretchr/testify/assert"
)

func TestFindDHCPOption(t *testing.T) {
	// Option 53 (msg type) = 1 (discover), then end marker.
	opts := []byte{53, 1, 1, 255}
	val := findDHCPOption(opts, 53)
	assert.Equal(t, []byte{1}, val)
}

func TestFindDHCPOption_NotFound(t *testing.T) {
	opts := []byte{53, 1, 1, 255}
	val := findDHCPOption(opts, 99)
	assert.Nil(t, val)
}

func TestFindDHCPOption_Empty(t *testing.T) {
	val := findDHCPOption([]byte{255}, 53)
	assert.Nil(t, val)
}

func TestPutDHCPOption(t *testing.T) {
	buf := make([]byte, 10)
	n := putDHCPOption(buf, 53, []byte{2})
	assert.Equal(t, 3, n)
	assert.Equal(t, byte(53), buf[0])
	assert.Equal(t, byte(1), buf[1])
	assert.Equal(t, byte(2), buf[2])
}

func TestBuildUDP_Length(t *testing.T) {
	payload := []byte("hello")
	udp := buildUDP(1234, 5678, payload)
	assert.Equal(t, 8+len(payload), len(udp))
	// Source port.
	assert.Equal(t, byte(0x04), udp[0])
	assert.Equal(t, byte(0xD2), udp[1])
	// Dest port.
	assert.Equal(t, byte(0x16), udp[2])
	assert.Equal(t, byte(0x2E), udp[3])
}
