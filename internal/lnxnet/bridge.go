// Package lnxnet implements userspace networking for the lnx VM.
package lnxnet

import (
	"fmt"
	"log/slog"
	"net"
	"syscall"
)

const (
	GatewayIP  = "192.168.64.1"
	GuestIP    = "192.168.64.2"
	SubnetMask = "255.255.255.0"
	MTU        = 1500
)

var gatewayMAC = net.HardwareAddr{0x02, 0x00, 0x00, 0x00, 0x00, 0x01}

// Bridge is a userspace network bridge between the VM and the host.
type Bridge struct {
	hostFd   int // our end — raw fd, used for read/write
	vmFd     int // VM end — raw fd, passed to vz (NEVER wrap in os.File)
	guestMAC net.HardwareAddr
}

// NewBridge creates a unix datagram socket pair and returns a Bridge.
// Neither fd is wrapped in os.File to avoid Go's runtime poller
// interfering with Virtualization.framework's dispatch-based I/O.
func NewBridge() (*Bridge, error) {
	fds, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_DGRAM, 0)
	if err != nil {
		return nil, fmt.Errorf("socketpair: %w", err)
	}

	for _, fd := range fds {
		syscall.SetsockoptInt(fd, syscall.SOL_SOCKET, syscall.SO_SNDBUF, 1*1024*1024)
		syscall.SetsockoptInt(fd, syscall.SOL_SOCKET, syscall.SO_RCVBUF, 4*1024*1024)
	}

	return &Bridge{
		hostFd: fds[0],
		vmFd:   fds[1],
	}, nil
}

// VMFd returns the raw fd for the VM end.
// Pass this to vz.NewFileHandleNetworkDeviceAttachmentWithFd.
// Do NOT wrap in os.File — Go's kqueue registration will steal
// readable events from VZ's dispatch_source.
func (b *Bridge) VMFd() int {
	return b.vmFd
}

// Start begins processing ethernet frames in a goroutine.
func (b *Bridge) Start() {
	go b.readLoop()
}

// Close shuts down the bridge.
func (b *Bridge) Close() {
	syscall.Close(b.hostFd)
	if b.vmFd >= 0 {
		syscall.Close(b.vmFd)
		b.vmFd = -1
	}
}

func (b *Bridge) readLoop() {
	buf := make([]byte, MTU+18)
	for {
		n, err := syscall.Read(b.hostFd, buf)
		if err != nil {
			slog.Debug("bridge readLoop exiting", "error", err)
			return
		}
		if n < 14 {
			continue
		}

		frame := make([]byte, n)
		copy(frame, buf[:n])

		slog.Debug("bridge rx", "len", n, "ethertype", fmt.Sprintf("0x%04x", uint16(frame[12])<<8|uint16(frame[13])))
		b.handleFrame(frame)
	}
}

func (b *Bridge) sendFrame(frame []byte) {
	go func() {
		n, err := syscall.Write(b.hostFd, frame)
		if err != nil {
			slog.Debug("bridge tx error", "error", err, "len", len(frame))
		} else {
			slog.Debug("bridge tx", "len", n)
		}
	}()
}
