//go:build darwin

package hv

import (
	"encoding/binary"
	"testing"
	"testing/synctest"
	"time"
)

// setupVsockTest creates a VirtioVsock with in-memory guest RAM and wired transport.
// Queue 0 = RX (device→guest), Queue 1 = TX (guest→device).
func setupVsockTest(t *testing.T) (*VirtioVsock, *VirtioMMIO, []byte, uint64) {
	t.Helper()
	const ramBase = 0x40000000

	vsock := NewVirtioVsock()
	mem := make([]byte, 1<<20)
	transport := NewVirtioMMIO(vsock, mem, ramBase, func(bool) {})
	vsock.SetTransport(transport)

	// Configure RX queue (queue 0): descriptors at 0x1000, avail at 0x2000, used at 0x3000.
	transport.Write(vioQueueSel, 4, 0)
	transport.Write(vioQueueNum, 4, 16)
	transport.Write(vioQueueDescLo, 4, uint64(uint32(ramBase+0x1000)))
	transport.Write(vioQueueDescHi, 4, 0)
	transport.Write(vioQueueDriverLo, 4, uint64(uint32(ramBase+0x2000)))
	transport.Write(vioQueueDriverHi, 4, 0)
	transport.Write(vioQueueDeviceLo, 4, uint64(uint32(ramBase+0x3000)))
	transport.Write(vioQueueDeviceHi, 4, 0)
	transport.Write(vioQueueReady, 4, 1)

	// Configure TX queue (queue 1): descriptors at 0x4000, avail at 0x5000, used at 0x6000.
	transport.Write(vioQueueSel, 4, 1)
	transport.Write(vioQueueNum, 4, 16)
	transport.Write(vioQueueDescLo, 4, uint64(uint32(ramBase+0x4000)))
	transport.Write(vioQueueDescHi, 4, 0)
	transport.Write(vioQueueDriverLo, 4, uint64(uint32(ramBase+0x5000)))
	transport.Write(vioQueueDriverHi, 4, 0)
	transport.Write(vioQueueDeviceLo, 4, uint64(uint32(ramBase+0x6000)))
	transport.Write(vioQueueDeviceHi, 4, 0)
	transport.Write(vioQueueReady, 4, 1)

	// Pre-populate the RX queue with writable buffers so sendPacket has descriptors.
	for i := 0; i < 8; i++ {
		bufOff := uint64(0x8000 + i*2048)
		writeDesc(mem, 0x1000, i, ramBase+bufOff, 2048, vdescFWrite, 0)
	}
	// Set avail ring: 8 entries available.
	for i := 0; i < 8; i++ {
		binary.LittleEndian.PutUint16(mem[0x2004+uint64(i)*2:], uint16(i))
	}
	binary.LittleEndian.PutUint16(mem[0x2002:], 8)

	return vsock, transport, mem, ramBase
}

// injectTXPacket builds a vsock packet in the TX queue and calls QueueNotify(1).
func injectTXPacket(t *testing.T, mem []byte, ramBase uint64, transport *VirtioMMIO, hdr vsockHdr, payload []byte) {
	t.Helper()
	pkt := serializeVsockHdr(hdr)
	pkt = append(pkt, payload...)

	// Place packet at 0x7000.
	copy(mem[0x7000:], pkt)

	// TX descriptor chain: single descriptor.
	writeDesc(mem, 0x4000, 0, ramBase+0x7000, uint32(len(pkt)), 0, 0)

	// Avail ring.
	idx := binary.LittleEndian.Uint16(mem[0x5002:])
	binary.LittleEndian.PutUint16(mem[0x5004+uint64(idx)*2:], 0)
	binary.LittleEndian.PutUint16(mem[0x5002:], idx+1)

	transport.Write(vioQueueNotify, 4, 1)
}

// readRXPacket reads the next packet from the RX used ring.
func readRXPacket(t *testing.T, mem []byte) (vsockHdr, []byte) {
	t.Helper()
	usedIdx := binary.LittleEndian.Uint16(mem[0x3002:])
	if usedIdx == 0 {
		t.Fatal("no packets in RX used ring")
	}

	// Read the first used entry.
	descIdx := binary.LittleEndian.Uint32(mem[0x3004:])
	written := binary.LittleEndian.Uint32(mem[0x3008:])

	// Find the buffer for this descriptor.
	bufOff := 0x8000 + int(descIdx)*2048
	data := mem[bufOff : bufOff+int(written)]

	if len(data) < vsockHdrSize {
		t.Fatalf("RX packet too short: %d bytes", len(data))
	}

	hdr := parseVsockHdr(data[:vsockHdrSize])
	payload := data[vsockHdrSize:]
	return hdr, payload
}

func TestVsockConfigRead_GuestCID(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		_, transport, _, _ := setupVsockTest(t)

		// Guest CID should be 3.
		cidLo := transport.Read(vioConfig+0, 4)
		cidHi := transport.Read(vioConfig+4, 4)
		cid := cidLo | (cidHi << 32)
		if cid != vsockCIDGuest {
			t.Fatalf("expected guest CID %d, got %d", vsockCIDGuest, cid)
		}
	})
}

func TestVsock_GuestConnectToListener(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		vsock, transport, mem, ramBase := setupVsockTest(t)

		// Host listens on port 1024.
		ln, err := vsock.Listen(1024)
		if err != nil {
			t.Fatal(err)
		}
		defer ln.Close()

		// Guest sends CONNECTION_REQUEST to host port 1024.
		injectTXPacket(t, mem, ramBase, transport, vsockHdr{
			srcCID:   vsockCIDGuest,
			dstCID:   vsockCIDHost,
			srcPort:  5000,
			dstPort:  1024,
			op:       vsockOpRequest,
			connType: vsockTypeStream,
			bufAlloc: 65536,
		}, nil)

		// Host should see a RESPONSE in the RX queue.
		hdr, _ := readRXPacket(t, mem)
		if hdr.op != vsockOpResponse {
			t.Fatalf("expected RESPONSE (op=%d), got op=%d", vsockOpResponse, hdr.op)
		}
		if hdr.dstPort != 5000 {
			t.Fatalf("expected dstPort 5000, got %d", hdr.dstPort)
		}

		// Accept should return a connection.
		connCh := make(chan bool, 1)
		go func() {
			conn, err := ln.Accept()
			connCh <- (err == nil && conn != nil)
			if conn != nil {
				conn.Close()
			}
		}()
		time.Sleep(10 * time.Millisecond)
		if !<-connCh {
			t.Fatal("Accept did not return a connection")
		}
	})
}

func TestVsock_GuestConnectNoListener(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		_, transport, mem, ramBase := setupVsockTest(t)

		// Guest connects to a port with no listener → should get RST.
		injectTXPacket(t, mem, ramBase, transport, vsockHdr{
			srcCID:   vsockCIDGuest,
			dstCID:   vsockCIDHost,
			srcPort:  5000,
			dstPort:  9999, // no listener
			op:       vsockOpRequest,
			connType: vsockTypeStream,
		}, nil)

		hdr, _ := readRXPacket(t, mem)
		if hdr.op != vsockOpRST {
			t.Fatalf("expected RST (op=%d), got op=%d", vsockOpRST, hdr.op)
		}
	})
}

func TestVsock_GuestSendsData(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		vsock, transport, mem, ramBase := setupVsockTest(t)

		ln, _ := vsock.Listen(1024)
		defer ln.Close()

		// Establish connection.
		injectTXPacket(t, mem, ramBase, transport, vsockHdr{
			srcCID: vsockCIDGuest, dstCID: vsockCIDHost,
			srcPort: 5000, dstPort: 1024,
			op: vsockOpRequest, connType: vsockTypeStream,
			bufAlloc: 65536,
		}, nil)

		// Consume the RESPONSE from RX used ring so the next readRXPacket works.
		_ = binary.LittleEndian.Uint16(mem[0x3002:])

		connCh := make(chan *vsockConn, 1)
		go func() {
			c, _ := ln.Accept()
			if c != nil {
				connCh <- c.(*vsockConn)
			}
		}()
		time.Sleep(10 * time.Millisecond)
		conn := <-connCh
		defer conn.Close()

		// Guest sends data.
		payload := []byte("hello from guest")
		injectTXPacket(t, mem, ramBase, transport, vsockHdr{
			srcCID: vsockCIDGuest, dstCID: vsockCIDHost,
			srcPort: 5000, dstPort: 1024,
			op: vsockOpRW, connType: vsockTypeStream,
			length: uint32(len(payload)),
		}, payload)

		// Host should be able to read the data.
		buf := make([]byte, 100)
		n, err := conn.Read(buf)
		if err != nil {
			t.Fatalf("Read: %v", err)
		}
		if string(buf[:n]) != "hello from guest" {
			t.Fatalf("expected 'hello from guest', got %q", string(buf[:n]))
		}
	})
}

func TestVsock_HostWritesToGuest(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		vsock, transport, mem, ramBase := setupVsockTest(t)

		ln, _ := vsock.Listen(1024)
		defer ln.Close()

		// Establish connection from guest.
		injectTXPacket(t, mem, ramBase, transport, vsockHdr{
			srcCID: vsockCIDGuest, dstCID: vsockCIDHost,
			srcPort: 5000, dstPort: 1024,
			op: vsockOpRequest, connType: vsockTypeStream,
			bufAlloc: 65536,
		}, nil)

		connCh := make(chan *vsockConn, 1)
		go func() {
			c, _ := ln.Accept()
			if c != nil {
				connCh <- c.(*vsockConn)
			}
		}()
		time.Sleep(10 * time.Millisecond)
		conn := <-connCh
		defer conn.Close()

		// Reset used ring tracking — skip the RESPONSE packet.
		binary.LittleEndian.PutUint16(mem[0x3002:], 0)
		// Re-fill RX avail ring.
		binary.LittleEndian.PutUint16(mem[0x2002:], 8)

		// Host writes data.
		n, err := conn.Write([]byte("hello from host"))
		if err != nil {
			t.Fatalf("Write: %v", err)
		}
		if n != 15 {
			t.Fatalf("expected 15 bytes written, got %d", n)
		}

		// Check that an RW packet appeared in the guest's RX queue.
		hdr, payload := readRXPacket(t, mem)
		if hdr.op != vsockOpRW {
			t.Fatalf("expected RW (op=%d), got op=%d", vsockOpRW, hdr.op)
		}
		if string(payload) != "hello from host" {
			t.Fatalf("expected 'hello from host', got %q", string(payload))
		}
	})
}

func TestVsock_Shutdown(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		vsock, transport, mem, ramBase := setupVsockTest(t)

		ln, _ := vsock.Listen(1024)
		defer ln.Close()

		// Establish connection.
		injectTXPacket(t, mem, ramBase, transport, vsockHdr{
			srcCID: vsockCIDGuest, dstCID: vsockCIDHost,
			srcPort: 5000, dstPort: 1024,
			op: vsockOpRequest, connType: vsockTypeStream,
			bufAlloc: 65536,
		}, nil)

		connCh := make(chan *vsockConn, 1)
		go func() {
			c, _ := ln.Accept()
			if c != nil {
				connCh <- c.(*vsockConn)
			}
		}()
		time.Sleep(10 * time.Millisecond)
		conn := <-connCh

		// Reset RX tracking.
		binary.LittleEndian.PutUint16(mem[0x3002:], 0)
		binary.LittleEndian.PutUint16(mem[0x2002:], 8)

		// Guest sends SHUTDOWN.
		injectTXPacket(t, mem, ramBase, transport, vsockHdr{
			srcCID: vsockCIDGuest, dstCID: vsockCIDHost,
			srcPort: 5000, dstPort: 1024,
			op: vsockOpShutdown, connType: vsockTypeStream,
			flags: 3,
		}, nil)

		// Host should see RST reply.
		hdr, _ := readRXPacket(t, mem)
		if hdr.op != vsockOpRST {
			t.Fatalf("expected RST after shutdown, got op=%d", hdr.op)
		}

		// Read should return EOF.
		buf := make([]byte, 10)
		_, err := conn.Read(buf)
		if err == nil {
			t.Fatal("expected EOF after shutdown, got nil error")
		}
	})
}

func TestVsock_ListenPortConflict(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		vsock, _, _, _ := setupVsockTest(t)

		ln, err := vsock.Listen(1024)
		if err != nil {
			t.Fatal(err)
		}
		defer ln.Close()

		_, err = vsock.Listen(1024)
		if err == nil {
			t.Fatal("expected error for duplicate listen on same port")
		}
	})
}

func TestVsockAddr_String(t *testing.T) {
	a := vsockAddr{cid: 3, port: 1024}
	if a.String() != "3:1024" {
		t.Fatalf("expected '3:1024', got %q", a.String())
	}
	if a.Network() != "vsock" {
		t.Fatalf("expected 'vsock', got %q", a.Network())
	}
}
