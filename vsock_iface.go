package lnx

import "net"

// VsockDevice abstracts vsock connectivity between host and guest.
// The VZ backend wraps *vz.VirtioSocketDevice; the QEMU backend uses
// Unix domain sockets via the virtio-vsock-pci device.
type VsockDevice interface {
	// Listen waits for guest connections on the given vsock port.
	// Returns a net.Listener whose Accept yields net.Conn streams.
	Listen(port uint32) (net.Listener, error)

	// Connect initiates a connection to the guest on the given vsock port.
	Connect(port uint32) (net.Conn, error)
}
