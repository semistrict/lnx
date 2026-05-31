//go:build darwin

package lnx

import (
	"net"

	vz "github.com/Code-Hex/vz/v3"
)

// vzVsockDevice wraps *vz.VirtioSocketDevice to implement VsockDevice.
type vzVsockDevice struct {
	dev *vz.VirtioSocketDevice
}

func (v *vzVsockDevice) Listen(port uint32) (net.Listener, error) {
	return v.dev.Listen(port)
}

func (v *vzVsockDevice) Connect(port uint32) (net.Conn, error) {
	return v.dev.Connect(port)
}
