//go:build darwin

package hv

import (
	"encoding/binary"
	"log/slog"
	"os"
)

// Virtio block device IDs and request types.
const (
	virtioDevBlk = 2

	blkTypeIn      = 0 // read
	blkTypeOut     = 1 // write
	blkTypeFlush   = 4
	blkTypeGetID   = 8
	blkTypeDiscard = 11
	blkStatusOK    = 0
	blkStatusIOErr = 1
	blkStatusUnsup = 2
)

// VirtioBlk implements a virtio block device backed by a host file.
type VirtioBlk struct {
	transport *VirtioMMIO
	file      *os.File
	capacity  uint64 // in 512-byte sectors
}

// NewVirtioBlk creates a block device backed by the given file.
// The transport must be set up before calling QueueNotify.
func NewVirtioBlk(f *os.File) (*VirtioBlk, error) {
	info, err := f.Stat()
	if err != nil {
		return nil, err
	}
	return &VirtioBlk{
		file:     f,
		capacity: uint64(info.Size()) / 512,
	}, nil
}

// SetTransport links this backend to its transport (called during setup).
func (b *VirtioBlk) SetTransport(t *VirtioMMIO) { b.transport = t }

func (b *VirtioBlk) DeviceID() uint32       { return virtioDevBlk }
func (b *VirtioBlk) DeviceFeatures() uint64  { return 0 } // no special features
func (b *VirtioBlk) ConfigWrite(offset uint64, size uint32, val uint64) {}

func (b *VirtioBlk) ConfigRead(offset uint64, size uint32) uint64 {
	// Config space: capacity at offset 0 (8 bytes, little-endian).
	switch {
	case offset == 0 && size == 4:
		return b.capacity & 0xffffffff
	case offset == 4 && size == 4:
		return b.capacity >> 32
	case offset == 0 && size == 8:
		return b.capacity
	default:
		return 0
	}
}

func (b *VirtioBlk) QueueNotify(qIdx uint32) {
	if qIdx != 0 {
		return
	}
	mem, ramBase := b.transport.Guest()
	q := b.transport.Queue(0)

	processed := 0
	for q.HasAvail(mem, ramBase) {
		head := q.PopAvail(mem, ramBase)
		written := b.processRequest(mem, ramBase, q, head)
		q.PutUsed(mem, ramBase, head, written)
		processed++
	}
	if processed > 0 {
		b.transport.RaiseIRQ()
	}
}

func (b *VirtioBlk) processRequest(mem []byte, ramBase uint64, q *Virtqueue, head uint16) uint32 {
	// Collect descriptor chain into read-only and write-only segments.
	var hdr []byte
	var dataBufs []bufRef
	var statusBuf []byte
	var totalWritten uint32

	q.WalkChain(mem, ramBase, head, func(desc VirtqDesc) {
		buf := GuestSlice(mem, ramBase, desc.Addr, desc.Len)
		if buf == nil {
			return
		}
		if desc.Flags&vdescFWrite == 0 {
			// Device-readable (from guest).
			if hdr == nil {
				hdr = buf
			} else {
				dataBufs = append(dataBufs, bufRef{buf, false})
			}
		} else {
			// Device-writable (to guest).
			if desc.Len == 1 {
				statusBuf = buf
			} else {
				dataBufs = append(dataBufs, bufRef{buf, true})
			}
		}
	})

	if len(hdr) < 16 || statusBuf == nil {
		slog.Error("virtio-blk: malformed request")
		return 0
	}

	reqType := binary.LittleEndian.Uint32(hdr[0:])
	sector := binary.LittleEndian.Uint64(hdr[8:])
	offset := int64(sector) * 512

	status := byte(blkStatusOK)

	switch reqType {
	case blkTypeIn: // read
		for _, db := range dataBufs {
			n, err := b.file.ReadAt(db.data, offset)
			if err != nil {
				slog.Error("virtio-blk read", "offset", offset, "error", err)
				status = blkStatusIOErr
				break
			}
			offset += int64(n)
			totalWritten += uint32(n)
		}
		totalWritten++ // status byte

	case blkTypeOut: // write
		for _, db := range dataBufs {
			n, err := b.file.WriteAt(db.data, offset)
			if err != nil {
				slog.Error("virtio-blk write", "offset", offset, "error", err)
				status = blkStatusIOErr
				break
			}
			offset += int64(n)
		}
		totalWritten = 1 // just the status byte

	case blkTypeFlush:
		if err := b.file.Sync(); err != nil {
			status = blkStatusIOErr
		}
		totalWritten = 1

	case blkTypeGetID:
		// Write device ID string to the first writable buffer.
		if len(dataBufs) > 0 && dataBufs[0].writable {
			id := []byte("lnx-hv-blk")
			copy(dataBufs[0].data, id)
			totalWritten = uint32(len(id)) + 1
		} else {
			totalWritten = 1
		}

	default:
		status = blkStatusUnsup
		totalWritten = 1
	}

	statusBuf[0] = status
	return totalWritten
}

type bufRef struct {
	data     []byte
	writable bool
}
