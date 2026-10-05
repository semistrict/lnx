//go:build darwin

package hv

import (
	"io"
	"log/slog"
	"sync"
)

// Virtio console device (ID 3). Provides hvc0 in the guest.
// Queue 0 = receiveq (host→guest), queue 1 = transmitq (guest→host).
const virtioDevConsole = 3

// VirtioConsole implements VirtioBackend for a simple single-port console.
type VirtioConsole struct {
	transport *VirtioMMIO
	out       io.Writer

	mu  sync.Mutex
	rxQ []byte // pending input for the guest
}

// NewVirtioConsole creates a console device that writes output to out.
func NewVirtioConsole(out io.Writer) *VirtioConsole {
	return &VirtioConsole{out: out}
}

func (c *VirtioConsole) SetTransport(t *VirtioMMIO) { c.transport = t }
func (c *VirtioConsole) DeviceID() uint32            { return virtioDevConsole }
func (c *VirtioConsole) DeviceFeatures() uint64       { return 0 }
func (c *VirtioConsole) ConfigWrite(offset uint64, size uint32, val uint64) {}

func (c *VirtioConsole) ConfigRead(offset uint64, size uint32) uint64 {
	// Config: cols(u16) rows(u16) max_nr_ports(u32) emerg_wr(u32)
	// Return 0 for all — no multiport, no size hints.
	return 0
}

// QueueNotify handles guest kicks.
func (c *VirtioConsole) QueueNotify(qIdx uint32) {
	switch qIdx {
	case 0:
		// Guest consumed RX buffers — nothing to do.
	case 1:
		c.processTX()
	}
}

// processTX reads console output from the guest TX queue and writes to out.
func (c *VirtioConsole) processTX() {
	mem, ramBase := c.transport.Guest()
	q := c.transport.Queue(1) // transmitq

	for q.HasAvail(mem, ramBase) {
		head := q.PopAvail(mem, ramBase)

		var data []byte
		q.WalkChain(mem, ramBase, head, func(desc VirtqDesc) {
			if desc.Flags&vdescFWrite == 0 { // readable by device = guest output
				buf := GuestSlice(mem, ramBase, desc.Addr, desc.Len)
				if buf != nil {
					data = append(data, buf...)
				}
			}
		})

		if len(data) > 0 {
			c.out.Write(data)
		}

		q.PutUsed(mem, ramBase, head, 0)
	}

	c.transport.RaiseIRQ()
}

// QueueInput adds bytes to the console receive buffer and delivers them
// to the guest's receiveq. Safe to call from any goroutine.
func (c *VirtioConsole) QueueInput(data []byte) {
	c.mu.Lock()
	c.rxQ = append(c.rxQ, data...)
	c.mu.Unlock()
	c.deliverRX()
}

// deliverRX pushes pending input into the guest's RX queue.
func (c *VirtioConsole) deliverRX() {
	c.mu.Lock()
	if len(c.rxQ) == 0 {
		c.mu.Unlock()
		return
	}
	pending := c.rxQ
	c.rxQ = nil
	c.mu.Unlock()

	mem, ramBase := c.transport.Guest()
	q := c.transport.Queue(0) // receiveq

	if !q.HasAvail(mem, ramBase) {
		// No RX buffers available — put data back.
		c.mu.Lock()
		c.rxQ = append(pending, c.rxQ...)
		c.mu.Unlock()
		slog.Debug("virtio-console: no RX buffers, queued input")
		return
	}

	head := q.PopAvail(mem, ramBase)
	written := q.WalkWriteChain(mem, ramBase, head, pending)
	q.PutUsed(mem, ramBase, head, written)

	c.transport.RaiseIRQ()
}
