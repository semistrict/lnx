package lnx

import (
	"encoding/gob"
	"net"
	"testing"

	"github.com/semistrict/lnx/internal/protocol"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestControlProtocol_SetupDelivered(t *testing.T) {
	hostConn, guestConn := net.Pipe()
	defer hostConn.Close()
	defer guestConn.Close()

	setup := &protocol.Setup{
		CWD:         "/tmp",
		User:        "ramon",
		UID:         501,
		HomeDir:     "/Users/ramon",
		Env:         []string{"FOO=bar"},
		Hostname:    "default.lnx",
		SSHAgent:    true,
		DirectShare: true,
		NestedDrives: []protocol.NestedDrive{
			{InstanceName: "default.default", DevicePath: "/dev/vdc"},
		},
		SyncShares: []string{"/sync"},
	}

	go func() {
		enc := gob.NewEncoder(hostConn)
		enc.Encode(protocol.Msg{Setup: setup})
	}()

	dec := gob.NewDecoder(guestConn)
	var msg protocol.Msg
	require.NoError(t, dec.Decode(&msg))
	require.NotNil(t, msg.Setup)
	assert.Equal(t, "/tmp", msg.Setup.CWD)
	assert.Equal(t, "ramon", msg.Setup.User)
	assert.Equal(t, 501, msg.Setup.UID)
	assert.Equal(t, "/Users/ramon", msg.Setup.HomeDir)
	assert.Equal(t, []string{"FOO=bar"}, msg.Setup.Env)
	assert.Equal(t, "default.lnx", msg.Setup.Hostname)
	assert.True(t, msg.Setup.SSHAgent)
	assert.True(t, msg.Setup.DirectShare)
	require.Len(t, msg.Setup.NestedDrives, 1)
	assert.Equal(t, "default.default", msg.Setup.NestedDrives[0].InstanceName)
	assert.Equal(t, "/dev/vdc", msg.Setup.NestedDrives[0].DevicePath)
	require.Len(t, msg.Setup.SyncShares, 1)
	assert.Equal(t, "/sync", msg.Setup.SyncShares[0])
}

func TestControlProtocol_SignalDelivered(t *testing.T) {
	hostConn, guestConn := net.Pipe()
	defer hostConn.Close()
	defer guestConn.Close()

	go func() {
		enc := gob.NewEncoder(hostConn)
		enc.Encode(protocol.Msg{Signal: &protocol.Signal{Sig: 2}})
	}()

	dec := gob.NewDecoder(guestConn)
	var msg protocol.Msg
	require.NoError(t, dec.Decode(&msg))
	require.NotNil(t, msg.Signal)
	assert.Equal(t, 2, msg.Signal.Sig)
}

func TestControlProtocol_ResizeDelivered(t *testing.T) {
	hostConn, guestConn := net.Pipe()
	defer hostConn.Close()
	defer guestConn.Close()

	go func() {
		enc := gob.NewEncoder(hostConn)
		enc.Encode(protocol.Msg{Resize: &protocol.Resize{Rows: 24, Cols: 80}})
	}()

	dec := gob.NewDecoder(guestConn)
	var msg protocol.Msg
	require.NoError(t, dec.Decode(&msg))
	require.NotNil(t, msg.Resize)
	assert.Equal(t, uint16(24), msg.Resize.Rows)
	assert.Equal(t, uint16(80), msg.Resize.Cols)
}
