package lnxnet

import (
	"encoding/binary"
	"fmt"
	"io"
	"log/slog"
	"net"
	"sync"
)

type tcpConnKey struct {
	SrcPort uint16
	DstIP   string
	DstPort uint16
}

type tcpConn struct {
	hostConn net.Conn
	guestSeq uint32
	guestAck uint32
	hostSeq  uint32
	state    string // "syn_received", "established", "closed"
	mu       sync.Mutex
}

var (
	tcpConns   = make(map[tcpConnKey]*tcpConn)
	tcpConnsMu sync.Mutex
)

func (b *Bridge) handleTCP(eth *ethernetFrame, ip *ipv4Header) {
	if len(ip.Payload) < 20 {
		return
	}

	srcPort := binary.BigEndian.Uint16(ip.Payload[0:2])
	dstPort := binary.BigEndian.Uint16(ip.Payload[2:4])
	seqNum := binary.BigEndian.Uint32(ip.Payload[4:8])
	ackNum := binary.BigEndian.Uint32(ip.Payload[8:12])
	dataOff := int((ip.Payload[12] >> 4)) * 4
	flags := ip.Payload[13]

	flagSYN := flags&0x02 != 0
	flagACK := flags&0x10 != 0
	flagFIN := flags&0x01 != 0
	flagRST := flags&0x04 != 0

	key := tcpConnKey{SrcPort: srcPort, DstIP: ip.DstIP.String(), DstPort: dstPort}

	if flagRST {
		b.closeTCPConn(key)
		return
	}

	if flagSYN && !flagACK {
		// New connection — SYN from guest.
		go b.handleTCPSyn(key, ip, srcPort, dstPort, seqNum)
		return
	}

	tcpConnsMu.Lock()
	tc, ok := tcpConns[key]
	tcpConnsMu.Unlock()
	if !ok {
		// Unknown connection, send RST.
		b.sendTCPRST(ip.DstIP, ip.SrcIP, dstPort, srcPort, ackNum)
		return
	}

	tc.mu.Lock()
	defer tc.mu.Unlock()

	if flagFIN {
		// Guest closing connection.
		tc.guestSeq = seqNum + 1
		// ACK the FIN.
		b.sendTCPPacket(ip.DstIP, ip.SrcIP, dstPort, srcPort, tc.hostSeq, tc.guestSeq, 0x10, nil) // ACK
		// Send our FIN.
		b.sendTCPPacket(ip.DstIP, ip.SrcIP, dstPort, srcPort, tc.hostSeq, tc.guestSeq, 0x11, nil) // FIN+ACK
		tc.hostSeq++
		tc.state = "closed"
		if tc.hostConn != nil {
			tc.hostConn.Close()
		}
		return
	}

	// Data from guest.
	if dataOff < len(ip.Payload) {
		data := ip.Payload[dataOff:]
		if len(data) > 0 && tc.hostConn != nil {
			tc.hostConn.Write(data)
			tc.guestSeq = seqNum + uint32(len(data))
			// ACK the data.
			b.sendTCPPacket(ip.DstIP, ip.SrcIP, dstPort, srcPort, tc.hostSeq, tc.guestSeq, 0x10, nil)
		}
	}
}

func (b *Bridge) handleTCPSyn(key tcpConnKey, ip *ipv4Header, srcPort, dstPort uint16, guestSeq uint32) {
	dst := net.JoinHostPort(ip.DstIP.String(), fmt.Sprintf("%d", dstPort))

	hostConn, err := net.Dial("tcp", dst)
	if err != nil {
		slog.Debug("tcp dial failed", "dst", dst, "error", err)
		b.sendTCPRST(ip.DstIP, ip.SrcIP, dstPort, srcPort, 0)
		return
	}

	tc := &tcpConn{
		hostConn: hostConn,
		guestSeq: guestSeq + 1,
		hostSeq:  1000, // initial sequence number
		state:    "syn_received",
	}

	tcpConnsMu.Lock()
	tcpConns[key] = tc
	tcpConnsMu.Unlock()

	// Send SYN+ACK.
	b.sendTCPPacket(ip.DstIP, ip.SrcIP, dstPort, srcPort, tc.hostSeq, tc.guestSeq, 0x12, nil)
	tc.hostSeq++
	tc.state = "established"

	// Read from host, send to guest.
	go func() {
		defer b.closeTCPConn(key)

		buf := make([]byte, MTU-40) // leave room for IP+TCP headers
		for {
			n, err := hostConn.Read(buf)
			if n > 0 {
				tc.mu.Lock()
				b.sendTCPPacket(ip.DstIP, ip.SrcIP, dstPort, srcPort, tc.hostSeq, tc.guestSeq, 0x18, buf[:n]) // PSH+ACK
				tc.hostSeq += uint32(n)
				tc.mu.Unlock()
			}
			if err != nil {
				if err != io.EOF {
					slog.Debug("tcp read from host", "error", err)
				}
				// Send FIN to guest.
				tc.mu.Lock()
				b.sendTCPPacket(ip.DstIP, ip.SrcIP, dstPort, srcPort, tc.hostSeq, tc.guestSeq, 0x11, nil) // FIN+ACK
				tc.hostSeq++
				tc.mu.Unlock()
				return
			}
		}
	}()
}

func (b *Bridge) closeTCPConn(key tcpConnKey) {
	tcpConnsMu.Lock()
	tc, ok := tcpConns[key]
	delete(tcpConns, key)
	tcpConnsMu.Unlock()
	if ok && tc.hostConn != nil {
		tc.hostConn.Close()
	}
}

func (b *Bridge) sendTCPRST(srcIP, dstIP net.IP, srcPort, dstPort uint16, seq uint32) {
	b.sendTCPPacket(srcIP, dstIP, srcPort, dstPort, seq, 0, 0x14, nil) // RST+ACK
}

func (b *Bridge) sendTCPPacket(srcIP, dstIP net.IP, srcPort, dstPort uint16, seq, ack uint32, flags byte, payload []byte) {
	tcpLen := 20 + len(payload)
	tcp := make([]byte, tcpLen)
	binary.BigEndian.PutUint16(tcp[0:2], srcPort)
	binary.BigEndian.PutUint16(tcp[2:4], dstPort)
	binary.BigEndian.PutUint32(tcp[4:8], seq)
	binary.BigEndian.PutUint32(tcp[8:12], ack)
	tcp[12] = 0x50 // data offset = 5 (20 bytes)
	tcp[13] = flags
	binary.BigEndian.PutUint16(tcp[14:16], 65535) // window size
	copy(tcp[20:], payload)

	// TCP checksum (with pseudo-header).
	binary.BigEndian.PutUint16(tcp[16:18], tcpChecksum(srcIP.To4(), dstIP.To4(), tcp))

	ipPkt := buildIPv4(srcIP.To4(), dstIP.To4(), protoTCP, tcp)
	frame := buildEthernet(b.guestMAC, gatewayMAC, etherTypeIPv4, ipPkt)
	b.sendFrame(frame)
}

func tcpChecksum(srcIP, dstIP net.IP, tcpSegment []byte) uint16 {
	// Pseudo-header.
	var sum uint32
	sum += uint32(srcIP[0])<<8 | uint32(srcIP[1])
	sum += uint32(srcIP[2])<<8 | uint32(srcIP[3])
	sum += uint32(dstIP[0])<<8 | uint32(dstIP[1])
	sum += uint32(dstIP[2])<<8 | uint32(dstIP[3])
	sum += uint32(protoTCP)
	sum += uint32(len(tcpSegment))

	// Clear checksum field before computing.
	orig := binary.BigEndian.Uint16(tcpSegment[16:18])
	binary.BigEndian.PutUint16(tcpSegment[16:18], 0)

	// Sum TCP segment.
	for i := 0; i < len(tcpSegment)-1; i += 2 {
		sum += uint32(binary.BigEndian.Uint16(tcpSegment[i : i+2]))
	}
	if len(tcpSegment)%2 != 0 {
		sum += uint32(tcpSegment[len(tcpSegment)-1]) << 8
	}

	// Restore.
	binary.BigEndian.PutUint16(tcpSegment[16:18], orig)

	for sum > 0xffff {
		sum = (sum >> 16) + (sum & 0xffff)
	}
	return ^uint16(sum)
}
