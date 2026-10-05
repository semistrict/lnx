//go:build darwin

package lnx

import (
	"testing"
	"testing/synctest"
	"time"
)

func TestBridgeNetBackend_MAC(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		b := newBridgeNetBackend()
		defer b.Close()

		mac := b.MAC()
		// Should be locally administered (bit 1 of first byte set).
		if mac[0]&0x02 == 0 {
			t.Fatalf("expected locally administered MAC, got %02x:%02x:%02x:%02x:%02x:%02x",
				mac[0], mac[1], mac[2], mac[3], mac[4], mac[5])
		}
	})
}

func TestBridgeNetBackend_ReadPacketEmpty(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		b := newBridgeNetBackend()
		defer b.Close()

		buf := make([]byte, 1500)
		n, err := b.ReadPacket(buf)
		if err != nil {
			t.Fatalf("unexpected error: %v", err)
		}
		if n != 0 {
			t.Fatalf("expected 0 bytes, got %d", n)
		}
	})
}

func TestBridgeNetBackend_EnqueueAndRead(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		b := newBridgeNetBackend()
		defer b.Close()

		// Simulate a frame from the bridge to the guest.
		frame := []byte{0x01, 0x02, 0x03, 0x04}
		b.enqueueRX(frame)

		// Ready channel should signal.
		select {
		case <-b.Ready():
		case <-time.After(time.Second):
			t.Fatal("ready channel not signaled")
		}

		buf := make([]byte, 1500)
		n, err := b.ReadPacket(buf)
		if err != nil {
			t.Fatalf("unexpected error: %v", err)
		}
		if n != 4 {
			t.Fatalf("expected 4 bytes, got %d", n)
		}
		if buf[0] != 1 || buf[3] != 4 {
			t.Fatalf("data mismatch")
		}

		// Second read should return 0.
		n, _ = b.ReadPacket(buf)
		if n != 0 {
			t.Fatalf("expected 0 bytes on second read, got %d", n)
		}
	})
}

func TestBridgeNetBackend_WritePacket(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		b := newBridgeNetBackend()
		defer b.Close()

		// WritePacket sends to the bridge which processes it.
		// An ARP request for the gateway should get an ARP reply back.
		// Build a minimal ARP request.
		guestMAC := b.GuestMAC()
		arpReq := buildARPRequest(guestMAC, [4]byte{192, 168, 64, 2}, [4]byte{192, 168, 64, 1})

		err := b.WritePacket(arpReq)
		if err != nil {
			t.Fatalf("WritePacket: %v", err)
		}

		// The bridge should respond with an ARP reply (enqueued via enqueueRX).
		// Give the bridge a moment to process.
		time.Sleep(10 * time.Millisecond)

		buf := make([]byte, 1500)
		n, _ := b.ReadPacket(buf)
		if n == 0 {
			t.Fatal("expected ARP reply from bridge, got nothing")
		}

		// Verify it's an Ethernet frame with EtherType 0x0806 (ARP).
		if n < 14 {
			t.Fatalf("frame too short: %d", n)
		}
		etherType := uint16(buf[12])<<8 | uint16(buf[13])
		if etherType != 0x0806 {
			t.Fatalf("expected ARP ethertype 0x0806, got 0x%04x", etherType)
		}

		// Check ARP operation = reply (2).
		if n >= 21 {
			op := uint16(buf[20])<<8 | uint16(buf[21])
			if op != 2 {
				t.Fatalf("expected ARP reply (op=2), got op=%d", op)
			}
		}
	})
}

// buildARPRequest builds a raw ARP who-has request ethernet frame.
func buildARPRequest(srcMAC []byte, srcIP, targetIP [4]byte) []byte {
	frame := make([]byte, 42) // 14 ethernet + 28 ARP

	// Ethernet header.
	copy(frame[0:6], []byte{0xff, 0xff, 0xff, 0xff, 0xff, 0xff}) // dst: broadcast
	copy(frame[6:12], srcMAC)                                      // src
	frame[12] = 0x08                                                // EtherType: ARP
	frame[13] = 0x06

	// ARP.
	frame[14] = 0x00; frame[15] = 0x01 // hardware type: Ethernet
	frame[16] = 0x08; frame[17] = 0x00 // protocol type: IPv4
	frame[18] = 6                       // hardware size
	frame[19] = 4                       // protocol size
	frame[20] = 0x00; frame[21] = 0x01 // operation: request
	copy(frame[22:28], srcMAC)          // sender MAC
	copy(frame[28:32], srcIP[:])        // sender IP
	// target MAC: zeros (unknown)
	copy(frame[38:42], targetIP[:]) // target IP

	return frame
}
