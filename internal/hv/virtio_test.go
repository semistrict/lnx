//go:build darwin

package hv

import (
	"encoding/binary"
	"testing"
	"testing/synctest"
)

// testBackend is a minimal VirtioBackend for testing the transport.
type testBackend struct {
	deviceID uint32
	features uint64
	config   [8]byte
	notified []uint32
}

func (b *testBackend) DeviceID() uint32                              { return b.deviceID }
func (b *testBackend) DeviceFeatures() uint64                        { return b.features }
func (b *testBackend) ConfigRead(offset uint64, size uint32) uint64  { return 0 }
func (b *testBackend) ConfigWrite(offset uint64, size uint32, v uint64) {}
func (b *testBackend) QueueNotify(qIdx uint32)                       { b.notified = append(b.notified, qIdx) }

func TestVirtioMMIO_Magic(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		backend := &testBackend{deviceID: 2}
		mem := make([]byte, 4096)
		v := NewVirtioMMIO(backend, mem, 0x40000000, func(bool) {})

		magic := v.Read(vioMagic, 4)
		if magic != vioMagicVal {
			t.Fatalf("expected magic 0x%x, got 0x%x", vioMagicVal, magic)
		}

		version := v.Read(vioVersion, 4)
		if version != vioVersionV2 {
			t.Fatalf("expected version %d, got %d", vioVersionV2, version)
		}

		devID := v.Read(vioDeviceID, 4)
		if devID != 2 {
			t.Fatalf("expected deviceID 2, got %d", devID)
		}
	})
}

func TestVirtioMMIO_FeatureNegotiation(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		backend := &testBackend{deviceID: 2, features: 0x1234}
		mem := make([]byte, 4096)
		v := NewVirtioMMIO(backend, mem, 0x40000000, func(bool) {})

		// Read low 32 bits of device features.
		v.Write(vioDevFeaturesSel, 4, 0)
		f0 := v.Read(vioDevFeatures, 4)
		if f0 != 0x1234 {
			t.Fatalf("expected features low 0x1234, got 0x%x", f0)
		}

		// Read high 32 bits (should include VIRTIO_F_VERSION_1 bit 0).
		v.Write(vioDevFeaturesSel, 4, 1)
		f1 := v.Read(vioDevFeatures, 4)
		if f1&1 == 0 {
			t.Fatal("expected VIRTIO_F_VERSION_1 in high features")
		}
	})
}

func TestVirtioMMIO_StatusReset(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		backend := &testBackend{deviceID: 2}
		mem := make([]byte, 4096)
		v := NewVirtioMMIO(backend, mem, 0x40000000, func(bool) {})

		// Set status.
		v.Write(vioStatus, 4, vioStatusAck|vioStatusDriver)
		status := v.Read(vioStatus, 4)
		if status != vioStatusAck|vioStatusDriver {
			t.Fatalf("expected status 0x3, got 0x%x", status)
		}

		// Reset by writing 0.
		v.Write(vioStatus, 4, 0)
		status = v.Read(vioStatus, 4)
		if status != 0 {
			t.Fatalf("expected status 0 after reset, got 0x%x", status)
		}
	})
}

func TestVirtioMMIO_QueueSetup(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		backend := &testBackend{deviceID: 2}
		mem := make([]byte, 4096)
		v := NewVirtioMMIO(backend, mem, 0x40000000, func(bool) {})

		// Select queue 0.
		v.Write(vioQueueSel, 4, 0)

		// Check max queue size.
		maxNum := v.Read(vioQueueNumMax, 4)
		if maxNum != 256 {
			t.Fatalf("expected QueueNumMax 256, got %d", maxNum)
		}

		// Configure queue.
		v.Write(vioQueueNum, 4, 128)
		v.Write(vioQueueDescLo, 4, 0x1000)
		v.Write(vioQueueDescHi, 4, 0)
		v.Write(vioQueueDriverLo, 4, 0x2000)
		v.Write(vioQueueDriverHi, 4, 0)
		v.Write(vioQueueDeviceLo, 4, 0x3000)
		v.Write(vioQueueDeviceHi, 4, 0)
		v.Write(vioQueueReady, 4, 1)

		// Verify queue is ready.
		ready := v.Read(vioQueueReady, 4)
		if ready != 1 {
			t.Fatal("expected queue ready")
		}

		// Verify internal state.
		q := v.Queue(0)
		if q.num != 128 {
			t.Fatalf("expected num=128, got %d", q.num)
		}
		if q.descAddr != 0x1000 {
			t.Fatalf("expected descAddr=0x1000, got 0x%x", q.descAddr)
		}
	})
}

func TestVirtioMMIO_QueueNotify(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		backend := &testBackend{deviceID: 2}
		mem := make([]byte, 4096)
		v := NewVirtioMMIO(backend, mem, 0x40000000, func(bool) {})

		// Notify queue 0.
		v.Write(vioQueueNotify, 4, 0)

		if len(backend.notified) != 1 || backend.notified[0] != 0 {
			t.Fatalf("expected notify for queue 0, got %v", backend.notified)
		}
	})
}

func TestVirtioMMIO_InterruptACK(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		irqLevel := false
		backend := &testBackend{deviceID: 2}
		mem := make([]byte, 4096)
		v := NewVirtioMMIO(backend, mem, 0x40000000, func(level bool) { irqLevel = level })

		// Raise IRQ.
		v.RaiseIRQ()
		if !irqLevel {
			t.Fatal("expected IRQ asserted")
		}

		intStatus := v.Read(vioIntStatus, 4)
		if intStatus&vioIntUsedRing == 0 {
			t.Fatal("expected used-ring interrupt status")
		}

		// ACK the interrupt.
		v.Write(vioIntACK, 4, vioIntUsedRing)
		if irqLevel {
			t.Fatal("expected IRQ deasserted after ACK")
		}

		intStatus = v.Read(vioIntStatus, 4)
		if intStatus != 0 {
			t.Fatalf("expected interrupt status 0, got 0x%x", intStatus)
		}
	})
}

// TestVirtqueue_PutUsed tests writing to the used ring.
func TestVirtqueue_PutUsed(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		const ramBase = 0x40000000
		mem := make([]byte, 65536)

		q := &Virtqueue{
			num:        16,
			ready:      true,
			descAddr:   ramBase + 0x1000,
			driverAddr: ramBase + 0x2000, // avail ring
			deviceAddr: ramBase + 0x3000, // used ring
		}

		// Used ring starts at offset 0x3000 in mem.
		// Format: flags(2) + idx(2) + entries(8 each)
		q.PutUsed(mem, ramBase, 5, 512)

		// Check used ring idx incremented.
		usedIdx := binary.LittleEndian.Uint16(mem[0x3002:])
		if usedIdx != 1 {
			t.Fatalf("expected used idx 1, got %d", usedIdx)
		}

		// Check entry: id=5, len=512.
		entryID := binary.LittleEndian.Uint32(mem[0x3004:])
		entryLen := binary.LittleEndian.Uint32(mem[0x3008:])
		if entryID != 5 || entryLen != 512 {
			t.Fatalf("expected entry (5, 512), got (%d, %d)", entryID, entryLen)
		}
	})
}

// TestVirtqueue_AvailRing tests reading from the available ring.
func TestVirtqueue_AvailRing(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		const ramBase = 0x40000000
		mem := make([]byte, 65536)

		q := &Virtqueue{
			num:        16,
			ready:      true,
			descAddr:   ramBase + 0x1000,
			driverAddr: ramBase + 0x2000,
			deviceAddr: ramBase + 0x3000,
		}

		// Not available initially.
		if q.HasAvail(mem, ramBase) {
			t.Fatal("expected no available entries")
		}

		// Simulate guest writing to avail ring:
		// avail ring at 0x2000: flags(2) + idx(2) + ring entries(2 each)
		binary.LittleEndian.PutUint16(mem[0x2002:], 1)  // idx = 1
		binary.LittleEndian.PutUint16(mem[0x2004:], 7)  // ring[0] = descriptor 7

		if !q.HasAvail(mem, ramBase) {
			t.Fatal("expected available entry")
		}

		head := q.PopAvail(mem, ramBase)
		if head != 7 {
			t.Fatalf("expected descriptor 7, got %d", head)
		}

		if q.HasAvail(mem, ramBase) {
			t.Fatal("expected no more available entries")
		}
	})
}

// TestVirtqueue_DescriptorChain tests walking a descriptor chain.
func TestVirtqueue_DescriptorChain(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		const ramBase = 0x40000000
		mem := make([]byte, 65536)

		q := &Virtqueue{
			num:        16,
			ready:      true,
			descAddr:   ramBase + 0x1000,
			driverAddr: ramBase + 0x2000,
			deviceAddr: ramBase + 0x3000,
		}

		// Write a 3-descriptor chain at descriptor table offset 0x1000.
		// Desc 0: addr=0x40004000, len=16, flags=NEXT, next=1
		writeDesc(mem, 0x1000, 0, ramBase+0x4000, 16, vdescFNext, 1)
		// Desc 1: addr=0x40005000, len=512, flags=WRITE|NEXT, next=2
		writeDesc(mem, 0x1000, 1, ramBase+0x5000, 512, vdescFWrite|vdescFNext, 2)
		// Desc 2: addr=0x40006000, len=1, flags=WRITE, next=0
		writeDesc(mem, 0x1000, 2, ramBase+0x6000, 1, vdescFWrite, 0)

		var descs []VirtqDesc
		q.WalkChain(mem, ramBase, 0, func(d VirtqDesc) {
			descs = append(descs, d)
		})

		if len(descs) != 3 {
			t.Fatalf("expected 3 descriptors, got %d", len(descs))
		}
		if descs[0].Len != 16 {
			t.Fatalf("desc[0].Len = %d, want 16", descs[0].Len)
		}
		if descs[1].Flags&vdescFWrite == 0 {
			t.Fatal("desc[1] should be writable")
		}
		if descs[2].Len != 1 {
			t.Fatalf("desc[2].Len = %d, want 1", descs[2].Len)
		}
	})
}

func writeDesc(mem []byte, tableOff int, idx int, addr uint64, length uint32, flags uint16, next uint16) {
	off := tableOff + idx*16
	binary.LittleEndian.PutUint64(mem[off:], addr)
	binary.LittleEndian.PutUint32(mem[off+8:], length)
	binary.LittleEndian.PutUint16(mem[off+12:], flags)
	binary.LittleEndian.PutUint16(mem[off+14:], next)
}
