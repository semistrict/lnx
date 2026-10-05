//go:build darwin

package hv

import (
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"sync"
	"time"
)

const (
	virtioDevVsock = 19

	vsockCIDHost  = 2
	vsockCIDGuest = 3 // matches VF backend convention

	vsockTypeStream = 1

	vsockOpInvalid       = 0
	vsockOpRequest       = 1
	vsockOpResponse      = 2
	vsockOpRST           = 3
	vsockOpShutdown      = 4
	vsockOpRW            = 5
	vsockOpCreditUpdate  = 6
	vsockOpCreditRequest = 7

	vsockHdrSize = 44

	vsockBufSize = 64 * 1024 // per-connection buffer
)

// VirtioVsock implements a virtio-vsock device (VIRTIO device ID 19).
// It provides the VsockDevice interface (Listen/Connect) for the lnx protocol.
type VirtioVsock struct {
	transport *VirtioMMIO

	mu        sync.Mutex
	listeners map[uint32]*vsockListener // host port → listener
	conns     map[vsockConnKey]*vsockConn
	pending   map[vsockConnKey]chan error // host-initiated Connect handshake

	rxMu sync.Mutex // protects RX queue operations in sendPacket
}

type vsockConnKey struct {
	srcCID, dstCID   uint64
	srcPort, dstPort uint32
}

func NewVirtioVsock() *VirtioVsock {
	return &VirtioVsock{
		listeners: make(map[uint32]*vsockListener),
		conns:     make(map[vsockConnKey]*vsockConn),
		pending:   make(map[vsockConnKey]chan error),
	}
}

func (v *VirtioVsock) SetTransport(t *VirtioMMIO) { v.transport = t }
func (v *VirtioVsock) DeviceID() uint32            { return virtioDevVsock }
func (v *VirtioVsock) DeviceFeatures() uint64       { return 0 }
func (v *VirtioVsock) ConfigWrite(offset uint64, size uint32, val uint64) {}

func (v *VirtioVsock) ConfigRead(offset uint64, size uint32) uint64 {
	// Config: guest_cid (uint64 at offset 0).
	switch {
	case offset == 0 && size == 4:
		return vsockCIDGuest & 0xffffffff
	case offset == 4 && size == 4:
		return vsockCIDGuest >> 32
	case offset == 0 && size == 8:
		return vsockCIDGuest
	}
	return 0
}

// QueueNotify handles TX queue (queue 1) notifications from the guest.
func (v *VirtioVsock) QueueNotify(qIdx uint32) {
	slog.Debug("vsock: QueueNotify", "queue", qIdx)
	if qIdx != 1 {
		return
	}
	v.processTX()
}

func (v *VirtioVsock) processTX() {
	mem, ramBase := v.transport.Guest()
	q := v.transport.Queue(1) // TX queue

	processed := 0
	for q.HasAvail(mem, ramBase) {
		head := q.PopAvail(mem, ramBase)

		// Collect the full packet (header + payload).
		var pkt []byte
		q.WalkChain(mem, ramBase, head, func(desc VirtqDesc) {
			buf := GuestSlice(mem, ramBase, desc.Addr, desc.Len)
			if buf != nil {
				pkt = append(pkt, buf...)
			}
		})

		q.PutUsed(mem, ramBase, head, 0)
		processed++

		if len(pkt) >= vsockHdrSize {
			v.handlePacket(pkt)
		}
	}
	if processed > 0 {
		txStats := q.Stats(mem, ramBase)
		rxStats := v.transport.Queue(0).Stats(mem, ramBase)
		slog.Debug("vsock processTX done",
			"processed", processed,
			"txUsedIdx", txStats.UsedIdx, "txAvailIdx", txStats.AvailIdx,
			"rxUsedIdx", rxStats.UsedIdx, "rxAvailIdx", rxStats.AvailIdx)
	}
	v.transport.RaiseIRQ()
}

func (v *VirtioVsock) handlePacket(pkt []byte) {
	hdr := parseVsockHdr(pkt[:vsockHdrSize])
	payload := pkt[vsockHdrSize:]

	slog.Debug("vsock rx", "op", hdr.op, "src_port", hdr.srcPort, "dst_port", hdr.dstPort,
		"len", hdr.length, "payload", len(payload))

	switch hdr.op {
	case vsockOpRequest:
		v.handleConnect(hdr)
	case vsockOpResponse:
		v.handleResponse(hdr)
	case vsockOpRW:
		v.handleData(hdr, payload)
	case vsockOpCreditUpdate:
		v.handleCreditUpdate(hdr)
	case vsockOpCreditRequest:
		v.handleCreditRequest(hdr)
	case vsockOpShutdown:
		v.handleShutdown(hdr)
	case vsockOpRST:
		v.handleRST(hdr)
	default:
		slog.Debug("vsock: unknown op", "op", hdr.op)
	}
}

func (v *VirtioVsock) handleConnect(hdr vsockHdr) {
	// Guest is connecting to host port hdr.dstPort.
	slog.Debug("vsock: handleConnect", "srcPort", hdr.srcPort, "dstPort", hdr.dstPort)
	v.mu.Lock()
	ln, ok := v.listeners[hdr.dstPort]
	v.mu.Unlock()

	if !ok {
		// No listener — send RST.
		v.sendPacket(vsockHdr{
			srcCID: vsockCIDHost, dstCID: hdr.srcCID,
			srcPort: hdr.dstPort, dstPort: hdr.srcPort,
			op: vsockOpRST, connType: vsockTypeStream,
		}, nil)
		return
	}

	key := vsockConnKey{hdr.srcCID, vsockCIDHost, hdr.srcPort, hdr.dstPort}
	conn := &vsockConn{
		dev:      v,
		key:      key,
		readCh:   make(chan []byte, 64),
		peerBuf:  hdr.bufAlloc,
		peerFwd:  hdr.fwdCnt,
		localBuf: vsockBufSize,
	}

	v.mu.Lock()
	v.conns[key] = conn
	v.mu.Unlock()

	// Send RESPONSE.
	v.sendPacket(vsockHdr{
		srcCID: vsockCIDHost, dstCID: hdr.srcCID,
		srcPort: hdr.dstPort, dstPort: hdr.srcPort,
		op: vsockOpResponse, connType: vsockTypeStream,
		bufAlloc: vsockBufSize,
	}, nil)

	// Deliver to listener.
	ln.deliver(conn)
}

func (v *VirtioVsock) handleResponse(hdr vsockHdr) {
	// Response to a host-initiated Connect.
	// Key: from the host's perspective, src=guest, dst=host.
	key := vsockConnKey{hdr.srcCID, vsockCIDHost, hdr.srcPort, hdr.dstPort}
	v.mu.Lock()
	ch, ok := v.pending[key]
	if ok {
		conn := v.conns[key]
		if conn != nil {
			conn.peerBuf = hdr.bufAlloc
			conn.peerFwd = hdr.fwdCnt
		}
		delete(v.pending, key)
	}
	v.mu.Unlock()
	if ok {
		ch <- nil
	}
}

func (v *VirtioVsock) handleData(hdr vsockHdr, payload []byte) {
	key := vsockConnKey{hdr.srcCID, vsockCIDHost, hdr.srcPort, hdr.dstPort}
	v.mu.Lock()
	conn, ok := v.conns[key]
	v.mu.Unlock()
	if !ok {
		return
	}

	if len(payload) > 0 {
		data := make([]byte, len(payload))
		copy(data, payload)
		select {
		case conn.readCh <- data:
			conn.mu.Lock()
			conn.localFwd += uint32(len(data))
			conn.mu.Unlock()
		default:
			slog.Debug("vsock: conn read buffer full, dropping")
		}
	}
}

func (v *VirtioVsock) handleCreditUpdate(hdr vsockHdr) {
	key := vsockConnKey{hdr.srcCID, vsockCIDHost, hdr.srcPort, hdr.dstPort}
	v.mu.Lock()
	conn, ok := v.conns[key]
	v.mu.Unlock()
	if !ok {
		return
	}
	conn.mu.Lock()
	conn.peerBuf = hdr.bufAlloc
	conn.peerFwd = hdr.fwdCnt
	conn.mu.Unlock()
}

func (v *VirtioVsock) handleCreditRequest(hdr vsockHdr) {
	key := vsockConnKey{hdr.srcCID, vsockCIDHost, hdr.srcPort, hdr.dstPort}
	v.mu.Lock()
	conn, ok := v.conns[key]
	v.mu.Unlock()
	if !ok {
		return
	}
	conn.mu.Lock()
	fwd := conn.localFwd
	conn.mu.Unlock()

	v.sendPacket(vsockHdr{
		srcCID: vsockCIDHost, dstCID: hdr.srcCID,
		srcPort: hdr.dstPort, dstPort: hdr.srcPort,
		op: vsockOpCreditUpdate, connType: vsockTypeStream,
		bufAlloc: vsockBufSize, fwdCnt: fwd,
	}, nil)
}

func (v *VirtioVsock) handleShutdown(hdr vsockHdr) {
	key := vsockConnKey{hdr.srcCID, vsockCIDHost, hdr.srcPort, hdr.dstPort}
	v.mu.Lock()
	conn, ok := v.conns[key]
	v.mu.Unlock()
	if !ok {
		return
	}
	conn.closeReadCh()

	// Send RST to complete teardown.
	v.sendPacket(vsockHdr{
		srcCID: vsockCIDHost, dstCID: hdr.srcCID,
		srcPort: hdr.dstPort, dstPort: hdr.srcPort,
		op: vsockOpRST, connType: vsockTypeStream,
	}, nil)

	v.mu.Lock()
	delete(v.conns, key)
	v.mu.Unlock()
}

func (v *VirtioVsock) handleRST(hdr vsockHdr) {
	key := vsockConnKey{hdr.srcCID, vsockCIDHost, hdr.srcPort, hdr.dstPort}
	v.mu.Lock()
	conn, ok := v.conns[key]
	delete(v.conns, key)
	ch, pending := v.pending[key]
	delete(v.pending, key)
	v.mu.Unlock()
	if ok {
		conn.closeReadCh()
	}
	if pending {
		ch <- errors.New("connection refused")
	}
}

// sendPacket injects a packet into the guest's RX queue.
// Thread-safe: rxMu serializes all RX queue operations since sendPacket
// can be called from multiple goroutines (vsock conn.Write, Connect handshake).
func (v *VirtioVsock) sendPacket(hdr vsockHdr, payload []byte) {
	slog.Debug("vsock sendPacket", "op", hdr.op, "dst_port", hdr.dstPort, "payload", len(payload))

	v.rxMu.Lock()
	mem, ramBase := v.transport.Guest()
	q := v.transport.Queue(0) // RX queue

	slog.Debug("vsock sendPacket RX state",
		"ready", q.Ready(), "num", q.Num(),
		"deviceAddr", fmt.Sprintf("0x%x", q.DeviceAddr()),
		"descAddr", fmt.Sprintf("0x%x", q.DescAddr()),
		"hasAvail", q.HasAvail(mem, ramBase))

	if !q.HasAvail(mem, ramBase) {
		v.rxMu.Unlock()
		slog.Error("vsock: RX queue full, dropping packet",
			"op", hdr.op, "dst_port", hdr.dstPort,
			"qReady", q.ready, "qNum", q.num,
			"qDescAddr", fmt.Sprintf("0x%x", q.descAddr),
			"qDriverAddr", fmt.Sprintf("0x%x", q.driverAddr),
			"qDeviceAddr", fmt.Sprintf("0x%x", q.deviceAddr),
			"qLastAvail", q.lastAvailIdx,
			"qAvailIdx", q.cAvailIdx(mem, ramBase))
		return
	}

	head := q.PopAvail(mem, ramBase)
	hdr.length = uint32(len(payload))
	hdrBytes := serializeVsockHdr(hdr)

	// Build contiguous packet (header + payload) for C vring write.
	var pkt []byte
	if len(payload) > 0 {
		pkt = make([]byte, vsockHdrSize+len(payload))
		copy(pkt, hdrBytes)
		copy(pkt[vsockHdrSize:], payload)
	} else {
		pkt = hdrBytes
	}

	// Use C vring implementation for correct ARM64 barrier ordering.
	written := q.WalkWriteChain(mem, ramBase, head, pkt)

	q.PutUsed(mem, ramBase, head, written)
	rxStats := q.Stats(mem, ramBase)
	v.rxMu.Unlock()

	slog.Debug("vsock sendPacket done",
		"op", hdr.op, "head", head, "written", written,
		"rxUsedIdx", rxStats.UsedIdx, "rxAvailIdx", rxStats.AvailIdx,
		"rxLastAvail", rxStats.LastAvailIdx)

	v.transport.RaiseIRQ()
}

// --- VsockDevice interface (Listen / Connect) ---

// Listen returns a net.Listener for the given host port.
func (v *VirtioVsock) Listen(port uint32) (net.Listener, error) {
	v.mu.Lock()
	defer v.mu.Unlock()

	if _, ok := v.listeners[port]; ok {
		return nil, errors.New("port already in use")
	}

	ln := &vsockListener{
		dev:     v,
		port:    port,
		connCh:  make(chan *vsockConn, 16),
		closeCh: make(chan struct{}),
	}
	v.listeners[port] = ln
	return ln, nil
}

// Connect initiates a connection from host to guest.
func (v *VirtioVsock) Connect(port uint32) (net.Conn, error) {
	// Allocate an ephemeral host port.
	v.mu.Lock()
	hostPort := uint32(49152)
	for {
		key := vsockConnKey{vsockCIDGuest, vsockCIDHost, port, hostPort}
		if _, ok := v.conns[key]; !ok {
			if _, ok := v.pending[key]; !ok {
				break
			}
		}
		hostPort++
		if hostPort > 65535 {
			v.mu.Unlock()
			return nil, errors.New("no ephemeral ports available")
		}
	}

	key := vsockConnKey{vsockCIDGuest, vsockCIDHost, port, hostPort}
	conn := &vsockConn{
		dev:      v,
		key:      key,
		readCh:   make(chan []byte, 64),
		localBuf: vsockBufSize,
	}
	v.conns[key] = conn

	// Register a pending-handshake channel so handleResponse/handleRST can wake us.
	handshakeCh := make(chan error, 1)
	v.pending[key] = handshakeCh
	v.mu.Unlock()

	// Send REQUEST to guest.
	v.sendPacket(vsockHdr{
		srcCID: vsockCIDHost, dstCID: vsockCIDGuest,
		srcPort: hostPort, dstPort: port,
		op: vsockOpRequest, connType: vsockTypeStream,
		bufAlloc: vsockBufSize,
	}, nil)

	// Wait for RESPONSE or RST from guest, with timeout.
	select {
	case err := <-handshakeCh:
		if err != nil {
			return nil, err
		}
		return conn, nil
	case <-time.After(5 * time.Second):
		v.mu.Lock()
		delete(v.conns, key)
		delete(v.pending, key)
		v.mu.Unlock()
		return nil, errors.New("connect timeout")
	}
}

// --- Packet serialization ---

type vsockHdr struct {
	srcCID, dstCID     uint64
	srcPort, dstPort   uint32
	length             uint32
	connType           uint16
	op                 uint16
	flags              uint32
	bufAlloc, fwdCnt   uint32
}

func parseVsockHdr(b []byte) vsockHdr {
	return vsockHdr{
		srcCID:   binary.LittleEndian.Uint64(b[0:]),
		dstCID:   binary.LittleEndian.Uint64(b[8:]),
		srcPort:  binary.LittleEndian.Uint32(b[16:]),
		dstPort:  binary.LittleEndian.Uint32(b[20:]),
		length:   binary.LittleEndian.Uint32(b[24:]),
		connType: binary.LittleEndian.Uint16(b[28:]),
		op:       binary.LittleEndian.Uint16(b[30:]),
		flags:    binary.LittleEndian.Uint32(b[32:]),
		bufAlloc: binary.LittleEndian.Uint32(b[36:]),
		fwdCnt:   binary.LittleEndian.Uint32(b[40:]),
	}
}

func serializeVsockHdr(h vsockHdr) []byte {
	b := make([]byte, vsockHdrSize)
	binary.LittleEndian.PutUint64(b[0:], h.srcCID)
	binary.LittleEndian.PutUint64(b[8:], h.dstCID)
	binary.LittleEndian.PutUint32(b[16:], h.srcPort)
	binary.LittleEndian.PutUint32(b[20:], h.dstPort)
	binary.LittleEndian.PutUint32(b[24:], h.length)
	binary.LittleEndian.PutUint16(b[28:], h.connType)
	binary.LittleEndian.PutUint16(b[30:], h.op)
	binary.LittleEndian.PutUint32(b[32:], h.flags)
	binary.LittleEndian.PutUint32(b[36:], h.bufAlloc)
	binary.LittleEndian.PutUint32(b[40:], h.fwdCnt)
	return b
}

// --- vsockListener ---

type vsockListener struct {
	dev       *VirtioVsock
	port      uint32
	connCh    chan *vsockConn
	closeCh   chan struct{}
	closeOnce sync.Once
}

func (l *vsockListener) deliver(c *vsockConn) {
	select {
	case l.connCh <- c:
	case <-l.closeCh:
	}
}

func (l *vsockListener) Accept() (net.Conn, error) {
	select {
	case c := <-l.connCh:
		return c, nil
	case <-l.closeCh:
		return nil, net.ErrClosed
	}
}

func (l *vsockListener) Close() error {
	l.closeOnce.Do(func() {
		close(l.closeCh)
		l.dev.mu.Lock()
		delete(l.dev.listeners, l.port)
		l.dev.mu.Unlock()
	})
	return nil
}

func (l *vsockListener) Addr() net.Addr {
	return vsockAddr{cid: vsockCIDHost, port: l.port}
}

// --- vsockConn (implements net.Conn) ---

type vsockConn struct {
	dev    *VirtioVsock
	key    vsockConnKey
	readCh chan []byte

	mu            sync.Mutex
	readBuf       []byte // partial read buffer
	localBuf      uint32
	localFwd      uint32
	peerBuf       uint32
	peerFwd       uint32
	txCnt         uint32
	closed        bool
	readChClosed  bool
}

func (c *vsockConn) closeReadCh() {
	c.mu.Lock()
	defer c.mu.Unlock()
	if !c.readChClosed {
		c.readChClosed = true
		close(c.readCh)
	}
}

func (c *vsockConn) Read(b []byte) (int, error) {
	// Drain leftover from previous read.
	if len(c.readBuf) > 0 {
		n := copy(b, c.readBuf)
		c.readBuf = c.readBuf[n:]
		return n, nil
	}

	data, ok := <-c.readCh
	if !ok {
		return 0, io.EOF
	}
	n := copy(b, data)
	if n < len(data) {
		c.readBuf = data[n:]
	}
	return n, nil
}

func (c *vsockConn) Write(b []byte) (int, error) {
	c.mu.Lock()
	if c.closed {
		c.mu.Unlock()
		return 0, net.ErrClosed
	}
	c.mu.Unlock()

	total := 0
	for len(b) > 0 {
		chunk := b
		if len(chunk) > 4096 {
			chunk = chunk[:4096]
		}

		c.mu.Lock()
		fwd := c.localFwd
		c.txCnt += uint32(len(chunk))
		c.mu.Unlock()

		c.dev.sendPacket(vsockHdr{
			srcCID: vsockCIDHost, dstCID: c.key.srcCID,
			srcPort: c.key.dstPort, dstPort: c.key.srcPort,
			op: vsockOpRW, connType: vsockTypeStream,
			bufAlloc: vsockBufSize, fwdCnt: fwd,
		}, chunk)

		total += len(chunk)
		b = b[len(chunk):]
	}
	return total, nil
}

func (c *vsockConn) Close() error {
	c.mu.Lock()
	if c.closed {
		c.mu.Unlock()
		return nil
	}
	c.closed = true
	fwd := c.localFwd
	c.mu.Unlock()

	c.dev.sendPacket(vsockHdr{
		srcCID: vsockCIDHost, dstCID: c.key.srcCID,
		srcPort: c.key.dstPort, dstPort: c.key.srcPort,
		op: vsockOpShutdown, connType: vsockTypeStream,
		flags: 3, // SHUTDOWN_RCV | SHUTDOWN_SEND
		bufAlloc: vsockBufSize, fwdCnt: fwd,
	}, nil)

	c.dev.mu.Lock()
	delete(c.dev.conns, c.key)
	c.dev.mu.Unlock()
	return nil
}

func (c *vsockConn) LocalAddr() net.Addr {
	return vsockAddr{cid: vsockCIDHost, port: c.key.dstPort}
}

func (c *vsockConn) RemoteAddr() net.Addr {
	return vsockAddr{cid: c.key.srcCID, port: c.key.srcPort}
}

func (c *vsockConn) SetDeadline(t time.Time) error      { return nil }
func (c *vsockConn) SetReadDeadline(t time.Time) error   { return nil }
func (c *vsockConn) SetWriteDeadline(t time.Time) error  { return nil }

type vsockAddr struct {
	cid  uint64
	port uint32
}

func (a vsockAddr) Network() string { return "vsock" }
func (a vsockAddr) String() string {
	return fmt.Sprintf("%d:%d", a.cid, a.port)
}
