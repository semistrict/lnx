package lnx

import (
	"net"

	vz "github.com/Code-Hex/vz/v3"
)

// vzVsock wraps *vz.VirtioSocketDevice to implement VsockDevice.
type vzVsock struct {
	dev *vz.VirtioSocketDevice
}

func newVZVsock(dev *vz.VirtioSocketDevice) VsockDevice {
	return &vzVsock{dev: dev}
}

func (v *vzVsock) Listen(port uint32) (net.Listener, error) {
	return v.dev.Listen(port)
}

func (v *vzVsock) Connect(port uint32) (net.Conn, error) {
	return v.dev.Connect(port)
}
