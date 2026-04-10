package lnx

import "net"

// VMState represents the state of a virtual machine.
type VMState int

const (
	VMStateStarting VMState = iota
	VMStateRunning
	VMStateStopped
	VMStateError
)

// VsockDevice abstracts vsock communication between host and guest.
// On Darwin it wraps vz.VirtioSocketDevice; on Linux it implements
// the Firecracker vsock Unix socket protocol.
type VsockDevice interface {
	// Listen creates a listener for incoming guest connections on the given port.
	Listen(port uint32) (net.Listener, error)
	// Connect establishes a connection to the guest on the given port.
	Connect(port uint32) (net.Conn, error)
}

// VirtualMachine abstracts VM lifecycle operations.
// On Darwin it wraps vz.VirtualMachine; on Linux it manages a Firecracker process.
type VirtualMachine interface {
	Start() error
	Stop() error
	RequestStop() error
	// StateChangedNotify returns a channel that receives state transitions.
	StateChangedNotify() <-chan VMState
	VsockDevice() VsockDevice
}

// MemorySnapshotter is implemented by VM backends that can pause, snapshot,
// and resume a running machine state.
type MemorySnapshotter interface {
	Pause() error
	Resume() error
	CreateMemorySnapshot(statePath, memPath string) error
}
