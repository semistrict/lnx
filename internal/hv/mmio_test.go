//go:build darwin

package hv

import (
	"testing"
	"testing/synctest"
)

type mockMMIO struct {
	lastReadOff  uint64
	lastWriteOff uint64
	lastWriteVal uint64
	readVal      uint64
}

func (m *mockMMIO) Read(offset uint64, size uint32) uint64  { m.lastReadOff = offset; return m.readVal }
func (m *mockMMIO) Write(offset uint64, size uint32, val uint64) {
	m.lastWriteOff = offset
	m.lastWriteVal = val
}

func TestMMIOBus_Dispatch(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		bus := &MMIOBus{}
		dev1 := &mockMMIO{readVal: 42}
		dev2 := &mockMMIO{readVal: 99}

		bus.Register(0x1000, 0x100, dev1)
		bus.Register(0x2000, 0x200, dev2)

		// Read from dev1.
		var val uint64
		ok := bus.Dispatch(0x1010, 4, false, &val)
		if !ok {
			t.Fatal("expected dispatch to succeed")
		}
		if val != 42 {
			t.Fatalf("expected 42, got %d", val)
		}
		if dev1.lastReadOff != 0x10 {
			t.Fatalf("expected offset 0x10, got 0x%x", dev1.lastReadOff)
		}

		// Write to dev2.
		val = 0xDEAD
		ok = bus.Dispatch(0x2080, 4, true, &val)
		if !ok {
			t.Fatal("expected dispatch to succeed")
		}
		if dev2.lastWriteOff != 0x80 {
			t.Fatalf("expected offset 0x80, got 0x%x", dev2.lastWriteOff)
		}
		if dev2.lastWriteVal != 0xDEAD {
			t.Fatalf("expected val 0xDEAD, got 0x%x", dev2.lastWriteVal)
		}

		// Miss — address not in any range.
		ok = bus.Dispatch(0x9999, 4, false, &val)
		if ok {
			t.Fatal("expected dispatch to miss")
		}
	})
}
