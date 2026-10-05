//go:build darwin

package hv

import (
	"bytes"
	"testing"
	"testing/synctest"
)

func TestPL011_TX(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		var buf bytes.Buffer
		uart := NewPL011(&buf, func(level bool) {})

		// Write characters via the data register.
		uart.Write(pl011DR, 4, 'H')
		uart.Write(pl011DR, 4, 'i')

		if buf.String() != "Hi" {
			t.Fatalf("expected %q, got %q", "Hi", buf.String())
		}
	})
}

func TestPL011_TX_IRQ(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		var buf bytes.Buffer
		irqState := false
		uart := NewPL011(&buf, func(level bool) { irqState = level })

		// Enable TX interrupt.
		uart.Write(pl011IMSC, 4, intTX)

		// Write a character — should fire TX complete IRQ.
		uart.Write(pl011DR, 4, 'A')

		if !irqState {
			t.Fatal("expected IRQ asserted after TX")
		}

		// Acknowledge the interrupt.
		uart.Write(pl011ICR, 4, intTX)

		if irqState {
			t.Fatal("expected IRQ deasserted after ICR")
		}
	})
}

func TestPL011_RX(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		var buf bytes.Buffer
		uart := NewPL011(&buf, func(bool) {})

		// Flag register: RX FIFO should be empty.
		flags := uart.Read(pl011FR, 4)
		if flags&frRXFE == 0 {
			t.Fatal("expected RXFE set when no input")
		}

		// Queue input.
		uart.QueueInput([]byte("AB"))

		// Flag register: RX FIFO should not be empty.
		flags = uart.Read(pl011FR, 4)
		if flags&frRXFE != 0 {
			t.Fatal("expected RXFE clear when input available")
		}

		// Read characters.
		ch1 := uart.Read(pl011DR, 4)
		ch2 := uart.Read(pl011DR, 4)
		if ch1 != 'A' || ch2 != 'B' {
			t.Fatalf("expected 'A','B', got %c,%c", rune(ch1), rune(ch2))
		}

		// FIFO should be empty again.
		flags = uart.Read(pl011FR, 4)
		if flags&frRXFE == 0 {
			t.Fatal("expected RXFE set after reading all input")
		}
	})
}

func TestPL011_RX_IRQ(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		var buf bytes.Buffer
		irqState := false
		uart := NewPL011(&buf, func(level bool) { irqState = level })

		// Enable RX interrupt.
		uart.Write(pl011IMSC, 4, intRX)

		// Queue input — should trigger IRQ.
		uart.QueueInput([]byte("X"))

		if !irqState {
			t.Fatal("expected IRQ asserted when RX data available")
		}

		// Read the character — should clear RX raw interrupt.
		uart.Read(pl011DR, 4)

		if irqState {
			t.Fatal("expected IRQ deasserted after reading all RX data")
		}
	})
}

func TestPL011_MaskedInterrupt(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		var buf bytes.Buffer
		irqState := false
		uart := NewPL011(&buf, func(level bool) { irqState = level })

		// RX interrupt NOT enabled in IMSC.
		uart.QueueInput([]byte("Y"))

		if irqState {
			t.Fatal("IRQ should not fire when RX interrupt is masked")
		}

		// RIS should show the raw interrupt pending.
		ris := uart.Read(pl011RIS, 4)
		if ris&intRX == 0 {
			t.Fatal("expected RIS to show RX pending")
		}

		// MIS should be clear (masked).
		mis := uart.Read(pl011MIS, 4)
		if mis != 0 {
			t.Fatalf("expected MIS=0, got 0x%x", mis)
		}
	})
}

func TestPL011_PeripheralID(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		var buf bytes.Buffer
		uart := NewPL011(&buf, func(bool) {})

		// First peripheral ID byte at 0xfe0.
		id0 := uart.Read(pl011PeripID, 4)
		if id0 != 0x11 {
			t.Fatalf("expected PeriphID0=0x11, got 0x%x", id0)
		}
	})
}

func TestPL011_FlagRegister(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		var buf bytes.Buffer
		uart := NewPL011(&buf, func(bool) {})

		flags := uart.Read(pl011FR, 4)

		// TX FIFO empty should always be set (we flush immediately).
		if flags&frTXFE == 0 {
			t.Fatal("expected TXFE set")
		}
		// TX FIFO full should never be set.
		if flags&frTXFF != 0 {
			t.Fatal("expected TXFF clear")
		}
	})
}
