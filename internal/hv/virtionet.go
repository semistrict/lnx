//go:build darwin

package hv

import (
	"encoding/binary"
	"log/slog"
	"sync"
)

const (
	virtioDevNet = 1

	// virtio-net feature bits.
	virtioNetFMAC = 1 << 5 // device has given MAC address

	// virtio-net header size (no mergeable buffers).
	virtioNetHdrSize = 12
)

// NetBackend is the interface for a network backend (e.g. vmnet).
type NetBackend interface {
	MAC() [6]byte
	ReadPacket(buf []byte) (int, error)
	WritePacket(data []byte) error
	Ready() <-chan struct{}
	Close()
}

// VirtioNet implements a virtio network device backed by a NetBackend.
type VirtioNet struct {
	transport *VirtioMMIO
	net       NetBackend
	mac       [6]byte
	stopCh    chan struct{}
	doneCh    chan struct{} // closed when rxPump exits
	rxMu      sync.Mutex   // protects RX queue operations in injectRX
}

// NewVirtioNet creates a virtio-net device backed by the given network backend.
func NewVirtioNet(net NetBackend) *VirtioNet {
	return &VirtioNet{
		net:    net,
		mac:    net.MAC(),
		stopCh: make(chan struct{}),
		doneCh: make(chan struct{}),
	}
}

func (n *VirtioNet) SetTransport(t *VirtioMMIO) {
	n.transport = t
	// Start the RX pump goroutine.
	go n.rxPump()
}

func (n *VirtioNet) DeviceID() uint32      { return virtioDevNet }
func (n *VirtioNet) DeviceFeatures() uint64 { return virtioNetFMAC }
func (n *VirtioNet) ConfigWrite(offset uint64, size uint32, val uint64) {}

func (n *VirtioNet) ConfigRead(offset uint64, size uint32) uint64 {
	// Config space: MAC at offset 0 (6 bytes), status at offset 6 (2 bytes).
	switch {
	case offset < 6:
		// Return byte(s) of MAC address.
		var val uint64
		for i := uint32(0); i < size && offset+uint64(i) < 6; i++ {
			val |= uint64(n.mac[offset+uint64(i)]) << (8 * i)
		}
		return val
	case offset == 6 && size >= 2:
		return 1 // VIRTIO_NET_S_LINK_UP
	default:
		return 0
	}
}

// QueueNotify handles guest TX (queue 1) notifications.
func (n *VirtioNet) QueueNotify(qIdx uint32) {
	if qIdx != 1 {
		return // queue 0 is RX (device → guest), handled by rxPump
	}
	n.processTX()
}

func (n *VirtioNet) processTX() {
	mem, ramBase := n.transport.Guest()
	q := n.transport.Queue(1) // TX queue

	processed := 0
	for q.HasAvail(mem, ramBase) {
		head := q.PopAvail(mem, ramBase)

		// Collect the frame data from the descriptor chain.
		var frame []byte
		first := true
		q.WalkChain(mem, ramBase, head, func(desc VirtqDesc) {
			buf := GuestSlice(mem, ramBase, desc.Addr, desc.Len)
			if buf == nil {
				return
			}
			if first {
				// Skip the virtio-net header (12 bytes).
				if len(buf) > virtioNetHdrSize {
					frame = append(frame, buf[virtioNetHdrSize:]...)
				}
				first = false
			} else {
				frame = append(frame, buf...)
			}
		})

		if len(frame) > 0 {
			if err := n.net.WritePacket(frame); err != nil {
				slog.Error("virtio-net: vmnet write failed", "error", err)
			}
		}

		q.PutUsed(mem, ramBase, head, 0)
		processed++
	}
	if processed > 0 {
		n.transport.RaiseIRQ()
	}
}

// rxPump reads packets from vmnet and injects them into the guest's RX queue.
func (n *VirtioNet) rxPump() {
	defer close(n.doneCh)
	buf := make([]byte, 65536)

	for {
		select {
		case <-n.stopCh:
			return
		case <-n.net.Ready():
		}

		// Drain all available packets.
		for {
			pktLen, err := n.net.ReadPacket(buf)
			if err != nil || pktLen == 0 {
				break
			}

			n.injectRX(buf[:pktLen])
		}
	}
}

func (n *VirtioNet) injectRX(frame []byte) {
	n.rxMu.Lock()
	mem, ramBase := n.transport.Guest()
	q := n.transport.Queue(0) // RX queue

	if !q.HasAvail(mem, ramBase) {
		n.rxMu.Unlock()
		slog.Debug("virtio-net: RX queue full, dropping packet")
		return
	}

	head := q.PopAvail(mem, ramBase)
	written := uint32(0)

	q.WalkChain(mem, ramBase, head, func(desc VirtqDesc) {
		if desc.Flags&vdescFWrite == 0 {
			return // skip device-readable descriptors
		}
		buf := GuestSlice(mem, ramBase, desc.Addr, desc.Len)
		if buf == nil {
			return
		}

		if written == 0 {
			// First writable descriptor: write virtio-net header + frame data.
			if uint32(len(buf)) < virtioNetHdrSize {
				return
			}
			// Zero the virtio-net header.
			for i := 0; i < virtioNetHdrSize; i++ {
				buf[i] = 0
			}
			// Set num_buffers = 1.
			binary.LittleEndian.PutUint16(buf[10:], 1)

			nn := copy(buf[virtioNetHdrSize:], frame)
			written = uint32(virtioNetHdrSize + nn)
		} else {
			// Continuation descriptor.
			nn := copy(buf, frame[written-virtioNetHdrSize:])
			written += uint32(nn)
		}
	})

	q.PutUsed(mem, ramBase, head, written)
	n.rxMu.Unlock()

	n.transport.RaiseIRQ()
}

// Close stops the rxPump goroutine and waits for it to finish.
func (n *VirtioNet) Close() {
	close(n.stopCh)
	<-n.doneCh
}
