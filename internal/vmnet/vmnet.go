//go:build darwin

package vmnet

// #cgo LDFLAGS: -framework vmnet
// #include "vmnet_helper.h"
//
// extern void vmnetPacketsAvailable(void *ctx);
//
// static void set_event_cb(vmnet_iface_t *iface, void *ctx) {
//     vmnet_helper_set_event_callback(iface, vmnetPacketsAvailable, ctx);
// }
import "C"
import (
	"fmt"
	"sync"
	"time"
	"unsafe"
)

// Interface wraps a vmnet.framework shared-mode (NAT) interface.
type Interface struct {
	iface   C.vmnet_iface_t
	mac     [6]byte
	mtu     int
	maxPkt  int
	readyCh chan struct{}

	mu     sync.Mutex
	closed bool
}

// NewInterface creates a vmnet shared-mode (NAT) interface.
// This may block if the entitlement is missing; use NewInterfaceTimeout for safety.
func New() (*Interface, error) {
	v := &Interface{readyCh: make(chan struct{}, 1)}

	// vmnet_start_interface can hang if entitlement is missing.
	// Run in a goroutine with a timeout.
	done := make(chan struct{})
	go func() {
		C.vmnet_helper_start(&v.iface)
		close(done)
	}()
	select {
	case <-done:
	case <-time.After(3 * time.Second):
		return nil, fmt.Errorf("vmnet start timed out — check com.apple.vm.networking entitlement")
	}

	if v.iface.status != 0 {
		return nil, fmt.Errorf("vmnet start failed (status %d) — check com.apple.vm.networking entitlement", v.iface.status)
	}

	v.mtu = int(v.iface.mtu)
	v.maxPkt = int(v.iface.max_pkt_size)

	// Parse MAC address string "xx:xx:xx:xx:xx:xx".
	macStr := C.GoString(&v.iface.mac[0])
	if _, err := fmt.Sscanf(macStr, "%02x:%02x:%02x:%02x:%02x:%02x",
		&v.mac[0], &v.mac[1], &v.mac[2], &v.mac[3], &v.mac[4], &v.mac[5]); err != nil {
		C.vmnet_helper_stop(&v.iface)
		return nil, fmt.Errorf("parse vmnet MAC %q: %w", macStr, err)
	}

	// Register callback for packet-available events.
	C.set_event_cb(&v.iface, unsafe.Pointer(v))

	return v, nil
}

// MAC returns the interface's MAC address.
func (v *Interface) MAC() [6]byte { return v.mac }

// MTU returns the interface MTU.
func (v *Interface) MTU() int { return v.mtu }

// MaxPacketSize returns the maximum packet size.
func (v *Interface) MaxPacketSize() int { return v.maxPkt }

// ReadPacket reads one Ethernet frame from vmnet.
// Returns the frame data or nil if no packet is available.
func (v *Interface) ReadPacket(buf []byte) (int, error) {
	var pktlen C.int
	rc := C.vmnet_helper_read(&v.iface, unsafe.Pointer(&buf[0]), C.int(len(buf)), &pktlen)
	if rc != 0 {
		return 0, nil // no packet available
	}
	return int(pktlen), nil
}

// WritePacket writes one Ethernet frame to vmnet.
func (v *Interface) WritePacket(data []byte) error {
	rc := C.vmnet_helper_write(&v.iface, unsafe.Pointer(&data[0]), C.int(len(data)))
	if rc != 0 {
		return fmt.Errorf("vmnet write failed")
	}
	return nil
}

// Ready returns a channel that signals when packets are available.
func (v *Interface) Ready() <-chan struct{} { return v.readyCh }

// Close shuts down the vmnet interface.
func (v *Interface) Close() {
	v.mu.Lock()
	defer v.mu.Unlock()
	if v.closed {
		return
	}
	v.closed = true
	C.vmnet_helper_stop(&v.iface)
}

//export vmnetPacketsAvailable
func vmnetPacketsAvailable(ctx unsafe.Pointer) {
	v := (*Interface)(ctx)
	select {
	case v.readyCh <- struct{}{}:
	default:
	}
}
