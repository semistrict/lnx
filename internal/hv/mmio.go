//go:build darwin

package hv

// MMIODevice handles reads and writes to a memory-mapped I/O region.
type MMIODevice interface {
	Read(offset uint64, size uint32) uint64
	Write(offset uint64, size uint32, val uint64)
}

// MMIOBus dispatches MMIO accesses to registered devices.
type MMIOBus struct {
	entries []mmioEntry
}

type mmioEntry struct {
	base, size uint64
	dev        MMIODevice
}

// Register adds a device at the given base address and size.
func (b *MMIOBus) Register(base, size uint64, dev MMIODevice) {
	b.entries = append(b.entries, mmioEntry{base, size, dev})
}

// Dispatch handles an MMIO access. Returns false if no device matched.
func (b *MMIOBus) Dispatch(addr uint64, size uint32, isWrite bool, val *uint64) bool {
	for i := range b.entries {
		e := &b.entries[i]
		if addr >= e.base && addr < e.base+e.size {
			offset := addr - e.base
			if isWrite {
				e.dev.Write(offset, size, *val)
			} else {
				*val = e.dev.Read(offset, size)
			}
			return true
		}
	}
	return false
}
