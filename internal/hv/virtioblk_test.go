//go:build darwin

package hv

import (
	"bytes"
	"encoding/binary"
	"os"
	"testing"
	"testing/synctest"
)

// setupBlkTest creates a virtio-blk device with a temp file backing and
// a fully wired transport. Returns the device, transport, guest memory,
// and the RAM base.
func setupBlkTest(t *testing.T, diskSize int) (*VirtioBlk, *VirtioMMIO, []byte, uint64) {
	t.Helper()
	const ramBase = 0x40000000

	f, err := os.CreateTemp(t.TempDir(), "blk-*.img")
	if err != nil {
		t.Fatal(err)
	}
	if err := f.Truncate(int64(diskSize)); err != nil {
		t.Fatal(err)
	}

	blk, err := NewVirtioBlk(f)
	if err != nil {
		t.Fatal(err)
	}

	mem := make([]byte, 1<<20) // 1MB guest RAM
	irqFired := false
	transport := NewVirtioMMIO(blk, mem, ramBase, func(level bool) { irqFired = level })
	blk.SetTransport(transport)
	_ = irqFired

	// Configure queue 0. Addresses must be GPAs (ramBase + offset).
	transport.Write(vioQueueSel, 4, 0)
	transport.Write(vioQueueNum, 4, 16)
	transport.Write(vioQueueDescLo, 4, uint64(uint32(ramBase+0x1000)))
	transport.Write(vioQueueDescHi, 4, uint64(uint32((ramBase+0x1000)>>32)))
	transport.Write(vioQueueDriverLo, 4, uint64(uint32(ramBase+0x2000)))
	transport.Write(vioQueueDriverHi, 4, uint64(uint32((ramBase+0x2000)>>32)))
	transport.Write(vioQueueDeviceLo, 4, uint64(uint32(ramBase+0x3000)))
	transport.Write(vioQueueDeviceHi, 4, uint64(uint32((ramBase+0x3000)>>32)))
	transport.Write(vioQueueReady, 4, 1)

	return blk, transport, mem, ramBase
}

// submitBlkReq sets up a virtio-blk request in guest memory and notifies.
func submitBlkReq(mem []byte, ramBase uint64, transport *VirtioMMIO,
	reqType uint32, sector uint64, dataBuf, statusBuf uint64, dataLen uint32, dataWritable bool) {

	// Write request header at 0x4000.
	binary.LittleEndian.PutUint32(mem[0x4000:], reqType)
	binary.LittleEndian.PutUint32(mem[0x4004:], 0) // reserved
	binary.LittleEndian.PutUint64(mem[0x4008:], sector)

	// Descriptor chain: header(read) → data(r/w) → status(write)
	dataFlags := uint16(0)
	if dataWritable {
		dataFlags = vdescFWrite
	}

	writeDesc(mem, 0x1000, 0, ramBase+0x4000, 16, vdescFNext, 1)
	writeDesc(mem, 0x1000, 1, ramBase+dataBuf, dataLen, dataFlags|vdescFNext, 2)
	writeDesc(mem, 0x1000, 2, ramBase+statusBuf, 1, vdescFWrite, 0)

	// Write to avail ring.
	idx := binary.LittleEndian.Uint16(mem[0x2002:])
	binary.LittleEndian.PutUint16(mem[0x2004+uint64(idx)*2:], 0) // head = desc 0
	binary.LittleEndian.PutUint16(mem[0x2002:], idx+1)

	// Notify.
	transport.Write(vioQueueNotify, 4, 0)
}

func TestVirtioBlk_Read(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		blk, transport, mem, ramBase := setupBlkTest(t, 1<<20)
		_ = blk

		// Write known data to the backing file at sector 0.
		pattern := bytes.Repeat([]byte("HELLO BLK TEST! "), 32) // 512 bytes
		blk.file.WriteAt(pattern, 0)

		// Submit a read request for sector 0.
		const dataBuf = 0x5000
		const statusBuf = 0x6000
		submitBlkReq(mem, ramBase, transport, blkTypeIn, 0, dataBuf, statusBuf, 512, true)

		// Check status byte.
		status := mem[statusBuf]
		if status != blkStatusOK {
			t.Fatalf("expected status OK (0), got %d", status)
		}

		// Check data was read correctly.
		got := mem[dataBuf : dataBuf+512]
		if !bytes.Equal(got, pattern) {
			t.Fatalf("read data mismatch: first 32 bytes: %x", got[:32])
		}
	})
}

func TestVirtioBlk_Write(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		blk, transport, mem, ramBase := setupBlkTest(t, 1<<20)

		// Put data to write at dataBuf.
		const dataBuf = 0x5000
		const statusBuf = 0x6000
		pattern := bytes.Repeat([]byte("WRITE TEST DATA!"), 32) // 512 bytes
		copy(mem[dataBuf:], pattern)

		// Submit a write request for sector 2.
		submitBlkReq(mem, ramBase, transport, blkTypeOut, 2, dataBuf, statusBuf, 512, false)

		status := mem[statusBuf]
		if status != blkStatusOK {
			t.Fatalf("expected status OK, got %d", status)
		}

		// Verify the data was written to the file.
		readBack := make([]byte, 512)
		blk.file.ReadAt(readBack, 2*512)
		if !bytes.Equal(readBack, pattern) {
			t.Fatal("written data mismatch")
		}
	})
}

func TestVirtioBlk_Capacity(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		_, transport, _, _ := setupBlkTest(t, 4096*512) // 2MB

		// Config space at offset 0x100: capacity in 512-byte sectors.
		capLo := transport.Read(vioConfig+0, 4)
		capHi := transport.Read(vioConfig+4, 4)
		capacity := capLo | (capHi << 32)
		if capacity != 4096 {
			t.Fatalf("expected capacity 4096 sectors, got %d", capacity)
		}
	})
}

func TestVirtioBlk_Flush(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		_, transport, mem, ramBase := setupBlkTest(t, 1<<20)

		const dataBuf = 0x5000
		const statusBuf = 0x6000
		submitBlkReq(mem, ramBase, transport, blkTypeFlush, 0, dataBuf, statusBuf, 0, true)

		status := mem[statusBuf]
		if status != blkStatusOK {
			t.Fatalf("expected flush status OK, got %d", status)
		}
	})
}

func TestVirtioBlk_UsedRing(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		_, transport, mem, ramBase := setupBlkTest(t, 1<<20)

		const dataBuf = 0x5000
		const statusBuf = 0x6000
		submitBlkReq(mem, ramBase, transport, blkTypeIn, 0, dataBuf, statusBuf, 512, true)

		// Used ring at 0x3000: flags(2) + idx(2) + entries.
		usedIdx := binary.LittleEndian.Uint16(mem[0x3002:])
		if usedIdx != 1 {
			t.Fatalf("expected used idx 1, got %d", usedIdx)
		}

		// IRQ should have been raised.
		intStatus := transport.Read(vioIntStatus, 4)
		if intStatus&vioIntUsedRing == 0 {
			t.Fatal("expected used-ring interrupt status after request completion")
		}
	})
}
