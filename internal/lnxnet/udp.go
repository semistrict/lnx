package lnxnet

import (
	"encoding/binary"
	"fmt"
	"log/slog"
	"net"
)

func (b *Bridge) handleUDP(eth *ethernetFrame, ip *ipv4Header) {
	if len(ip.Payload) < 8 {
		return
	}

	srcPort := binary.BigEndian.Uint16(ip.Payload[0:2])
	dstPort := binary.BigEndian.Uint16(ip.Payload[2:4])
	udpPayload := ip.Payload[8:]

	// DHCP: client (68) -> server (67).
	if srcPort == 68 && dstPort == 67 {
		b.handleDHCP(eth, ip, udpPayload)
		return
	}

	// DNS: forward to host resolver.
	if dstPort == 53 {
		b.handleDNS(eth, ip, srcPort, udpPayload)
		return
	}

	// Generic UDP: relay through host.
	go b.relayUDP(eth, ip, srcPort, dstPort, udpPayload)
}

func (b *Bridge) relayUDP(eth *ethernetFrame, ip *ipv4Header, srcPort, dstPort uint16, payload []byte) {
	dst := net.JoinHostPort(ip.DstIP.String(), itoa(dstPort))
	conn, err := net.Dial("udp", dst)
	if err != nil {
		slog.Debug("udp dial failed", "dst", dst, "error", err)
		return
	}
	defer conn.Close()

	if _, err := conn.Write(payload); err != nil {
		return
	}

	buf := make([]byte, MTU)
	n, err := conn.Read(buf)
	if err != nil {
		return
	}

	b.sendUDPReply(ip.DstIP, ip.SrcIP, dstPort, srcPort, buf[:n])
}

func (b *Bridge) handleDNS(eth *ethernetFrame, ip *ipv4Header, srcPort uint16, payload []byte) {
	go func() {
		conn, err := net.Dial("udp", "8.8.8.8:53")
		if err != nil {
			slog.Debug("dns dial failed", "error", err)
			return
		}
		defer conn.Close()

		if _, err := conn.Write(payload); err != nil {
			return
		}

		buf := make([]byte, MTU)
		n, err := conn.Read(buf)
		if err != nil {
			return
		}

		b.sendUDPReply(ip.DstIP, ip.SrcIP, 53, srcPort, buf[:n])
	}()
}

func (b *Bridge) sendUDPReply(srcIP, dstIP net.IP, srcPort, dstPort uint16, payload []byte) {
	udp := buildUDP(srcPort, dstPort, payload)
	ipPkt := buildIPv4(srcIP.To4(), dstIP.To4(), protoUDP, udp)
	frame := buildEthernet(b.guestMAC, gatewayMAC, etherTypeIPv4, ipPkt)
	b.sendFrame(frame)
}

func itoa(n uint16) string {
	return fmt.Sprintf("%d", n)
}
