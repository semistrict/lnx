//go:build darwin

package hv

import (
	"io"
	"sync"
)

// PL011 emulates an ARM PL011 UART — enough for Linux console I/O.
//
// Register map (offsets from base):
//   0x000 UARTDR   Data register
//   0x018 UARTFR   Flag register (read-only)
//   0x024 UARTIBRD Integer baud rate (ignored)
//   0x028 UARTFBRD Fractional baud rate (ignored)
//   0x02c UARTLCR_H Line control (ignored)
//   0x030 UARTCR   Control register (ignored)
//   0x038 UARTIMSC Interrupt mask
//   0x03c UARTRIS  Raw interrupt status
//   0x040 UARTMIS  Masked interrupt status
//   0x044 UARTICR  Interrupt clear
//   0xfe0 UARTPeriphID0..3

const (
	pl011DR      = 0x000
	pl011FR      = 0x018
	pl011IBRD    = 0x024
	pl011FBRD    = 0x028
	pl011LCR_H   = 0x02c
	pl011CR      = 0x030
	pl011IMSC    = 0x038
	pl011RIS     = 0x03c
	pl011MIS     = 0x040
	pl011ICR     = 0x044
	pl011PeripID = 0xfe0

	// Flag register bits.
	frRXFE = 1 << 4 // receive FIFO empty
	frTXFF = 1 << 5 // transmit FIFO full
	frTXFE = 1 << 7 // transmit FIFO empty

	// Interrupt bits.
	intRX = 1 << 4 // receive interrupt
	intTX = 1 << 5 // transmit interrupt
)

// PL011 peripheral IDs (ARM PrimeCell PL011).
var pl011IDs = [8]uint32{0x11, 0x10, 0x14, 0x00, 0x0d, 0xf0, 0x05, 0xb1}

// PL011 is a minimal PL011 UART for console I/O.
type PL011 struct {
	out     io.Writer  // TX output (host stdout)
	irqFunc func(bool) // assert/deassert IRQ

	mu   sync.Mutex
	rxQ  []byte // pending RX characters
	imsc uint32 // interrupt mask
	ris  uint32 // raw interrupt status
}

// NewPL011 creates a PL011 UART. out receives transmitted characters.
// irqFunc is called to assert (true) or deassert (false) the UART IRQ line.
func NewPL011(out io.Writer, irqFunc func(bool)) *PL011 {
	return &PL011{out: out, irqFunc: irqFunc}
}

// QueueInput adds characters to the receive FIFO. Call from any goroutine.
func (u *PL011) QueueInput(data []byte) {
	u.mu.Lock()
	u.rxQ = append(u.rxQ, data...)
	u.ris |= intRX
	u.mu.Unlock()
	u.updateIRQ()
}

func (u *PL011) Read(offset uint64, size uint32) uint64 {
	switch offset {
	case pl011DR:
		u.mu.Lock()
		var ch byte
		if len(u.rxQ) > 0 {
			ch = u.rxQ[0]
			u.rxQ = u.rxQ[1:]
			if len(u.rxQ) == 0 {
				u.ris &^= intRX
			}
		}
		u.mu.Unlock()
		u.updateIRQ()
		return uint64(ch)

	case pl011FR:
		u.mu.Lock()
		flags := uint64(frTXFE) // TX always ready
		if len(u.rxQ) == 0 {
			flags |= frRXFE
		}
		u.mu.Unlock()
		return flags

	case pl011IMSC:
		u.mu.Lock()
		v := uint64(u.imsc)
		u.mu.Unlock()
		return v

	case pl011RIS:
		u.mu.Lock()
		v := uint64(u.ris)
		u.mu.Unlock()
		return v

	case pl011MIS:
		u.mu.Lock()
		v := uint64(u.ris & u.imsc)
		u.mu.Unlock()
		return v

	default:
		// Peripheral ID registers at 0xfe0-0xffc.
		if offset >= pl011PeripID && offset < pl011PeripID+32 {
			idx := (offset - pl011PeripID) / 4
			if idx < uint64(len(pl011IDs)) {
				return uint64(pl011IDs[idx])
			}
		}
		return 0
	}
}

func (u *PL011) Write(offset uint64, size uint32, val uint64) {
	switch offset {
	case pl011DR:
		b := [1]byte{byte(val)}
		u.out.Write(b[:])
		// TX complete — set TX interrupt.
		u.mu.Lock()
		u.ris |= intTX
		u.mu.Unlock()
		u.updateIRQ()

	case pl011IMSC:
		u.mu.Lock()
		u.imsc = uint32(val)
		u.mu.Unlock()
		u.updateIRQ()

	case pl011ICR:
		u.mu.Lock()
		u.ris &^= uint32(val)
		u.mu.Unlock()
		u.updateIRQ()

	default:
		// IBRD, FBRD, LCR_H, CR — silently ignore.
	}
}

func (u *PL011) updateIRQ() {
	u.mu.Lock()
	pending := u.ris & u.imsc
	u.mu.Unlock()
	if pending != 0 {
		u.irqFunc(true)
	} else {
		u.irqFunc(false)
	}
}
