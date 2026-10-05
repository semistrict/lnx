//go:build darwin

package hv

import (
	"encoding/binary"
	"fmt"
	"log/slog"
	"sync/atomic"
)

// Virtio MMIO register offsets (v2 modern).
const (
	vioMagic          = 0x000 // 0x74726976
	vioVersion        = 0x004 // 2
	vioDeviceID       = 0x008
	vioVendorID       = 0x00c
	vioDevFeatures    = 0x010
	vioDevFeaturesSel = 0x014
	vioDrvFeatures    = 0x020
	vioDrvFeaturesSel = 0x024
	vioQueueSel       = 0x030
	vioQueueNumMax    = 0x034
	vioQueueNum       = 0x038
	vioQueueReady     = 0x044
	vioQueueNotify    = 0x050
	vioIntStatus      = 0x060
	vioIntACK         = 0x064
	vioStatus         = 0x070
	vioQueueDescLo    = 0x080
	vioQueueDescHi    = 0x084
	vioQueueDriverLo  = 0x090
	vioQueueDriverHi  = 0x094
	vioQueueDeviceLo  = 0x0a0
	vioQueueDeviceHi  = 0x0a4
	vioConfigGen      = 0x0fc
	vioConfig         = 0x100

	vioMagicVal  = 0x74726976
	vioVersionV2 = 2
	vioVendorVal = 0x554d4551 // "QEMU"

	// Status bits.
	vioStatusAck        = 1
	vioStatusDriver     = 2
	vioStatusDriverOK   = 4
	vioStatusFeaturesOK = 8

	// Feature bits.
	virtioFVersion1 = 1 << 32

	// Descriptor flags.
	vdescFNext  = 1
	vdescFWrite = 2

	// Interrupt status bits.
	vioIntUsedRing = 1
)

// VirtioBackend is implemented by each virtio device type (blk, vsock, etc.).
type VirtioBackend interface {
	DeviceID() uint32
	DeviceFeatures() uint64
	ConfigRead(offset uint64, size uint32) uint64
	ConfigWrite(offset uint64, size uint32, val uint64)
	QueueNotify(qIdx uint32)
}

// VirtioMMIO is a virtio MMIO transport instance.
type VirtioMMIO struct {
	backend VirtioBackend
	mem     []byte // guest RAM (host view)
	ramBase uint64 // guest physical address of mem[0]
	irqFunc func(bool)
	kickFn  func() // optional: kick vCPU out of WFI after RaiseIRQ

	devFeaturesSel uint32
	drvFeaturesSel uint32
	drvFeatures    uint64
	status         uint32
	intStatus      atomic.Uint32 // accessed from both vCPU and non-vCPU threads
	configGen      uint32

	queueSel uint32
	queues   [3]Virtqueue // blk=1, net=2, vsock=3
}

// Virtqueue holds state for one virtio queue.
type Virtqueue struct {
	num      uint32
	ready    bool
	descAddr uint64
	driverAddr uint64 // avail ring
	deviceAddr uint64 // used ring

	lastAvailIdx uint16
}

// Accessors for debug logging.
func (q *Virtqueue) Ready() bool       { return q.ready }
func (q *Virtqueue) Num() uint32       { return q.num }
func (q *Virtqueue) DescAddr() uint64  { return q.descAddr }
func (q *Virtqueue) DeviceAddr() uint64 { return q.deviceAddr }

// VirtqueueStats holds debug state for a virtqueue.
type VirtqueueStats struct {
	Ready        bool
	Num          uint32
	LastAvailIdx uint16
	AvailIdx     uint16
	UsedIdx      uint16
}

// Stats returns the current state of the virtqueue for debugging.
func (q *Virtqueue) Stats(mem []byte, ramBase uint64) VirtqueueStats {
	s := VirtqueueStats{
		Ready:        q.ready,
		Num:          q.num,
		LastAvailIdx: q.lastAvailIdx,
		AvailIdx:     q.AvailIdx(mem, ramBase),
	}
	// Read current used ring idx.
	base := gpa2hva(mem, ramBase, q.deviceAddr)
	if base != nil && len(base) >= 4 {
		s.UsedIdx = binary.LittleEndian.Uint16(base[2:])
	}
	return s
}

// NewVirtioMMIO creates a virtio MMIO transport for the given backend.
func NewVirtioMMIO(backend VirtioBackend, mem []byte, ramBase uint64, irqFunc func(bool)) *VirtioMMIO {
	return &VirtioMMIO{
		backend: backend,
		mem:     mem,
		ramBase: ramBase,
		irqFunc: irqFunc,
	}
}

func (v *VirtioMMIO) Read(offset uint64, size uint32) uint64 {
	switch offset {
	case vioMagic:
		return vioMagicVal
	case vioVersion:
		return vioVersionV2
	case vioDeviceID:
		return uint64(v.backend.DeviceID())
	case vioVendorID:
		return vioVendorVal
	case vioDevFeatures:
		f := v.backend.DeviceFeatures() | virtioFVersion1
		if v.devFeaturesSel == 1 {
			return f >> 32
		}
		return f & 0xffffffff
	case vioQueueNumMax:
		return 256
	case vioQueueReady:
		return boolU64(v.queue().ready)
	case vioIntStatus:
		s := v.intStatus.Load()
		if s != 0 {
			// Log queue stats at interrupt time for vsock (device 19).
			if v.backend.DeviceID() == 19 {
				rxQ := &v.queues[0]
				stats := rxQ.Stats(v.mem, v.ramBase)
				// Dump raw avail ring header.
				rawAvail := gpa2hva(v.mem, v.ramBase, rxQ.driverAddr)
				var availHdr [4]byte
				if rawAvail != nil && len(rawAvail) >= 4 {
					copy(availHdr[:], rawAvail[:4])
				}
				slog.Debug("virtio: intStatus read (vsock)",
					"status", s,
					"rxUsedIdx", stats.UsedIdx,
					"rxAvailIdx", stats.AvailIdx,
					"rawAvailHdr", fmt.Sprintf("%02x %02x %02x %02x", availHdr[0], availHdr[1], availHdr[2], availHdr[3]))
			} else {
				slog.Debug("virtio: intStatus read", "device", v.backend.DeviceID(), "status", s)
			}
		}
		return uint64(s)
	case vioStatus:
		return uint64(v.status)
	case vioConfigGen:
		return uint64(v.configGen)
	default:
		if offset >= vioConfig {
			return v.backend.ConfigRead(offset-vioConfig, size)
		}
		return 0
	}
}

func (v *VirtioMMIO) Write(offset uint64, size uint32, val uint64) {
	switch offset {
	case vioDevFeaturesSel:
		v.devFeaturesSel = uint32(val)
	case vioDrvFeatures:
		if v.drvFeaturesSel == 0 {
			v.drvFeatures = (v.drvFeatures & 0xffffffff00000000) | (val & 0xffffffff)
		} else {
			v.drvFeatures = (v.drvFeatures & 0xffffffff) | (val << 32)
		}
	case vioDrvFeaturesSel:
		v.drvFeaturesSel = uint32(val)
	case vioQueueSel:
		v.queueSel = uint32(val)
	case vioQueueNum:
		v.queue().num = uint32(val)
	case vioQueueReady:
		q := v.queue()
		q.ready = val != 0
		if val != 0 && v.backend.DeviceID() == 19 {
			slog.Debug("virtio: vsock queue ready",
				"queueSel", v.queueSel,
				"num", q.num,
				"descAddr", fmt.Sprintf("0x%x", q.descAddr),
				"driverAddr", fmt.Sprintf("0x%x", q.driverAddr),
				"deviceAddr", fmt.Sprintf("0x%x", q.deviceAddr))
		}
	case vioQueueNotify:
		v.backend.QueueNotify(uint32(val))
	case vioIntACK:
		if v.backend.DeviceID() == 19 {
			slog.Debug("virtio: intACK (vsock)", "val", val)
		}
		// Atomic And to avoid TOCTOU race with RaiseIRQ's intStatus.Or().
		// Without atomic clear, a sendPacket between Load and Store can set
		// intStatus to 1, which Store(0) then overwrites — losing the interrupt.
		old := v.intStatus.And(^uint32(val))
		if old&^uint32(val) == 0 {
			v.irqFunc(false)
		}
	case vioStatus:
		slog.Debug("virtio: status change", "device", v.backend.DeviceID(), "status", fmt.Sprintf("0x%x", val))
		v.status = uint32(val)
		if v.status == 0 {
			// Device reset.
			v.drvFeatures = 0
			v.intStatus.Store(0)
			for i := range v.queues {
				v.queues[i] = Virtqueue{}
			}
		}
	case vioQueueDescLo:
		q := v.queue()
		q.descAddr = (q.descAddr & 0xffffffff00000000) | val
	case vioQueueDescHi:
		q := v.queue()
		q.descAddr = (q.descAddr & 0xffffffff) | (val << 32)
	case vioQueueDriverLo:
		q := v.queue()
		q.driverAddr = (q.driverAddr & 0xffffffff00000000) | val
	case vioQueueDriverHi:
		q := v.queue()
		q.driverAddr = (q.driverAddr & 0xffffffff) | (val << 32)
	case vioQueueDeviceLo:
		q := v.queue()
		q.deviceAddr = (q.deviceAddr & 0xffffffff00000000) | val
	case vioQueueDeviceHi:
		q := v.queue()
		q.deviceAddr = (q.deviceAddr & 0xffffffff) | (val << 32)
	default:
		if offset >= vioConfig {
			v.backend.ConfigWrite(offset-vioConfig, size, val)
		}
	}
}

func (v *VirtioMMIO) queue() *Virtqueue {
	idx := v.queueSel
	if idx >= uint32(len(v.queues)) {
		idx = 0
	}
	return &v.queues[idx]
}

// RaiseIRQ sets the used-ring interrupt and signals the vCPU.
// Always calls irqFunc(true) to set the pending bitmap. The bitmap
// deduplicates: multiple RaiseIRQ calls for the same device only
// produce one edge pulse in drainPendingIRQs. reinjectPendingIRQs
// ensures delivery even after ExitCanceled consumes SetPendingInterrupt.
func (v *VirtioMMIO) RaiseIRQ() {
	v.intStatus.Or(vioIntUsedRing)
	v.irqFunc(true)
	if v.kickFn != nil {
		v.kickFn()
	}
}

// SetKickFn sets a callback invoked after RaiseIRQ to wake vCPUs
// that may be in WFI. Typically calls VCPU.ForceExit.
func (v *VirtioMMIO) SetKickFn(fn func()) { v.kickFn = fn }

// HasPendingIRQ returns true if this device has an unacknowledged interrupt.
func (v *VirtioMMIO) HasPendingIRQ() bool { return v.intStatus.Load() != 0 }

// Queue returns the Virtqueue at index i.
func (v *VirtioMMIO) Queue(i int) *Virtqueue { return &v.queues[i] }

// Guest returns the guest memory and ram base.
func (v *VirtioMMIO) Guest() ([]byte, uint64) { return v.mem, v.ramBase }

// --- Virtqueue operations ---

// gpa2hva converts a guest physical address to a host pointer into mem.
func gpa2hva(mem []byte, ramBase, gpa uint64) []byte {
	off := gpa - ramBase
	if off >= uint64(len(mem)) {
		return nil
	}
	return mem[off:]
}

// VirtqDesc is a parsed virtqueue descriptor.
type VirtqDesc struct {
	Addr  uint64
	Len   uint32
	Flags uint16
	Next  uint16
}

// ReadDesc reads descriptor at index from the descriptor table.
func (q *Virtqueue) ReadDesc(mem []byte, ramBase uint64, idx uint16) VirtqDesc {
	base := gpa2hva(mem, ramBase, q.descAddr+uint64(idx)*16)
	if base == nil || len(base) < 16 {
		return VirtqDesc{}
	}
	return VirtqDesc{
		Addr:  binary.LittleEndian.Uint64(base[0:]),
		Len:   binary.LittleEndian.Uint32(base[8:]),
		Flags: binary.LittleEndian.Uint16(base[12:]),
		Next:  binary.LittleEndian.Uint16(base[14:]),
	}
}

// AvailIdx returns the current available ring index.
func (q *Virtqueue) AvailIdx(mem []byte, ramBase uint64) uint16 {
	base := gpa2hva(mem, ramBase, q.driverAddr)
	if base == nil || len(base) < 4 {
		return 0
	}
	return binary.LittleEndian.Uint16(base[2:])
}

// AvailRing returns the descriptor index at available ring position pos.
func (q *Virtqueue) AvailRing(mem []byte, ramBase uint64, pos uint16) uint16 {
	off := 4 + uint64(pos%uint16(q.num))*2
	base := gpa2hva(mem, ramBase, q.driverAddr+off)
	if base == nil || len(base) < 2 {
		return 0
	}
	return binary.LittleEndian.Uint16(base[0:])
}

// HasAvail returns true if there are unprocessed entries in the available ring.
// Delegates to C vring implementation for correct ARM64 memory ordering.
func (q *Virtqueue) HasAvail(mem []byte, ramBase uint64) bool {
	return q.cHasAvail(mem, ramBase)
}

// PopAvail returns the next available descriptor head index and advances.
// Delegates to C vring implementation which includes smp_rmb() between
// reading the avail index and reading the ring entry — critical on ARM64
// to prevent stale descriptor reads (matches QEMU's virtqueue_split_pop).
func (q *Virtqueue) PopAvail(mem []byte, ramBase uint64) uint16 {
	return q.cPopAvail(mem, ramBase)
}

// PutUsed writes an entry to the used ring and increments the used index.
// Delegates to C vring implementation for correct barrier ordering.
func (q *Virtqueue) PutUsed(mem []byte, ramBase uint64, descIdx uint16, written uint32) {
	q.cPutUsed(mem, ramBase, descIdx, written)
}

// WalkChain walks a descriptor chain starting at head, calling fn for each descriptor.
func (q *Virtqueue) WalkChain(mem []byte, ramBase uint64, head uint16, fn func(VirtqDesc)) {
	idx := head
	for i := 0; i < int(q.num); i++ { // safety limit
		desc := q.ReadDesc(mem, ramBase, idx)
		fn(desc)
		if desc.Flags&vdescFNext == 0 {
			break
		}
		idx = desc.Next
	}
}

// WalkWriteChain writes data into WRITE-flagged descriptors in the chain.
// Returns total bytes written. Uses C implementation for correct barrier ordering.
// This is the device-to-guest (RX) path.
func (q *Virtqueue) WalkWriteChain(mem []byte, ramBase uint64, head uint16, data []byte) uint32 {
	return q.cWalkWriteChain(mem, ramBase, head, data)
}

// GuestSlice returns a host-accessible slice for a guest buffer.
func GuestSlice(mem []byte, ramBase, gpa uint64, length uint32) []byte {
	hva := gpa2hva(mem, ramBase, gpa)
	if hva == nil || uint32(len(hva)) < length {
		slog.Error("guest buffer out of range", "gpa", gpa, "len", length)
		return nil
	}
	return hva[:length]
}

func boolU64(b bool) uint64 {
	if b {
		return 1
	}
	return 0
}
