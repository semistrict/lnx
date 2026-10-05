//go:build darwin

package lnx

import (
	"net"
	"sync"

	"github.com/semistrict/lnx/internal/lnxnet"
)

// bridgeNetBackend adapts lnxnet.Bridge to the hv.NetBackend interface
// for use with the HV backend's virtio-net device.
type bridgeNetBackend struct {
	bridge *lnxnet.Bridge
	mac    [6]byte

	mu      sync.Mutex
	rxQueue [][]byte       // frames from bridge → guest
	readyCh chan struct{}
	closed  bool
}

func newBridgeNetBackend() *bridgeNetBackend {
	b := &bridgeNetBackend{
		readyCh: make(chan struct{}, 1),
	}
	// The bridge calls our writeFn when it has a frame for the guest.
	b.bridge = lnxnet.NewChannelBridge(b.enqueueRX)

	// Use the gateway MAC as our device MAC. The guest will see this
	// as its own NIC MAC. The bridge's DHCP will assign GuestIP.
	gwMAC := b.bridge.GatewayMAC()
	// Give the guest a different MAC from the gateway.
	b.mac = [6]byte{0x02, 0x00, 0x00, 0x00, 0x00, 0x02}
	_ = gwMAC
	return b
}

// enqueueRX is called by the bridge when it has a frame for the guest.
func (b *bridgeNetBackend) enqueueRX(frame []byte) {
	cp := make([]byte, len(frame))
	copy(cp, frame)

	b.mu.Lock()
	if b.closed {
		b.mu.Unlock()
		return
	}
	b.rxQueue = append(b.rxQueue, cp)
	b.mu.Unlock()

	select {
	case b.readyCh <- struct{}{}:
	default:
	}
}

func (b *bridgeNetBackend) MAC() [6]byte { return b.mac }

func (b *bridgeNetBackend) ReadPacket(buf []byte) (int, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	if len(b.rxQueue) == 0 {
		return 0, nil
	}
	frame := b.rxQueue[0]
	b.rxQueue = b.rxQueue[1:]
	n := copy(buf, frame)
	return n, nil
}

func (b *bridgeNetBackend) WritePacket(data []byte) error {
	b.bridge.HandleTX(data)
	return nil
}

func (b *bridgeNetBackend) Ready() <-chan struct{} { return b.readyCh }

func (b *bridgeNetBackend) Close() {
	b.mu.Lock()
	b.closed = true
	b.mu.Unlock()
	b.bridge.Close()
}

// GuestMAC returns the guest's MAC as a net.HardwareAddr.
func (b *bridgeNetBackend) GuestMAC() net.HardwareAddr {
	return net.HardwareAddr(b.mac[:])
}
