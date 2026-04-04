package lnxnet

import (
	"encoding/binary"
	"net"
)

const (
	dhcpDiscover = 1
	dhcpOffer    = 2
	dhcpRequest  = 3
	dhcpAck      = 5
)

// handleDHCP responds to DHCP discover/request messages.
func (b *Bridge) handleDHCP(eth *ethernetFrame, ip *ipv4Header, udpPayload []byte) {
	if len(udpPayload) < 240 {
		return
	}

	op := udpPayload[0]
	if op != 1 { // boot request
		return
	}

	xid := udpPayload[4:8]
	clientMAC := net.HardwareAddr(udpPayload[28:34])

	// Find DHCP message type in options (offset 240+).
	msgType := findDHCPOption(udpPayload[240:], 53)
	if len(msgType) == 0 {
		return
	}

	var replyType byte
	switch msgType[0] {
	case dhcpDiscover:
		replyType = dhcpOffer
	case dhcpRequest:
		replyType = dhcpAck
	default:
		return
	}

	guestIPBytes := net.ParseIP(GuestIP).To4()
	gatewayIPBytes := net.ParseIP(GatewayIP).To4()
	subnetBytes := net.ParseIP(SubnetMask).To4()

	// Build DHCP reply.
	reply := make([]byte, 300)
	reply[0] = 2 // boot reply
	reply[1] = 1 // ethernet
	reply[2] = 6 // hw addr len
	copy(reply[4:8], xid)
	copy(reply[16:20], guestIPBytes)   // yiaddr
	copy(reply[20:24], gatewayIPBytes) // siaddr
	copy(reply[28:34], clientMAC)
	// Magic cookie.
	copy(reply[236:240], []byte{99, 130, 83, 99})

	// DHCP options.
	opts := reply[240:]
	i := 0
	i += putDHCPOption(opts[i:], 53, []byte{replyType})                                // msg type
	i += putDHCPOption(opts[i:], 1, subnetBytes)                                       // subnet mask
	i += putDHCPOption(opts[i:], 3, gatewayIPBytes)                                    // router
	i += putDHCPOption(opts[i:], 6, append([]byte{8, 8, 8, 8}, []byte{8, 8, 4, 4}...)) // DNS
	i += putDHCPOption(opts[i:], 51, []byte{0, 0, 0xFF, 0xFF})                         // lease time (infinite)
	i += putDHCPOption(opts[i:], 54, gatewayIPBytes)                                   // server ID
	opts[i] = 255                                                                      // end
	i++

	dhcpReply := reply[:240+i]

	// Wrap in UDP (src 67 -> dst 68).
	udp := buildUDP(67, 68, dhcpReply)

	// Wrap in IP.
	ipPkt := buildIPv4(gatewayIPBytes, net.IPv4bcast.To4(), protoUDP, udp)

	// Wrap in ethernet — broadcast.
	frame := buildEthernet(
		net.HardwareAddr{0xff, 0xff, 0xff, 0xff, 0xff, 0xff},
		gatewayMAC,
		etherTypeIPv4,
		ipPkt,
	)
	b.sendFrame(frame)
}

func findDHCPOption(opts []byte, code byte) []byte {
	for i := 0; i < len(opts); {
		if opts[i] == 255 { // end
			return nil
		}
		if opts[i] == 0 { // pad
			i++
			continue
		}
		if i+1 >= len(opts) {
			return nil
		}
		optCode := opts[i]
		optLen := int(opts[i+1])
		i += 2
		if i+optLen > len(opts) {
			return nil
		}
		if optCode == code {
			return opts[i : i+optLen]
		}
		i += optLen
	}
	return nil
}

func putDHCPOption(buf []byte, code byte, data []byte) int {
	buf[0] = code
	buf[1] = byte(len(data))
	copy(buf[2:], data)
	return 2 + len(data)
}

func buildUDP(srcPort, dstPort uint16, payload []byte) []byte {
	udp := make([]byte, 8+len(payload))
	binary.BigEndian.PutUint16(udp[0:2], srcPort)
	binary.BigEndian.PutUint16(udp[2:4], dstPort)
	binary.BigEndian.PutUint16(udp[4:6], uint16(8+len(payload)))
	// checksum = 0 (optional for UDP over IPv4)
	copy(udp[8:], payload)
	return udp
}

func buildIPv4(srcIP, dstIP net.IP, protocol uint8, payload []byte) []byte {
	totalLen := 20 + len(payload)
	pkt := make([]byte, totalLen)
	pkt[0] = 0x45 // version 4, IHL 5
	binary.BigEndian.PutUint16(pkt[2:4], uint16(totalLen))
	pkt[8] = 64 // TTL
	pkt[9] = protocol
	copy(pkt[12:16], srcIP)
	copy(pkt[16:20], dstIP)

	// Compute header checksum.
	var sum uint32
	for i := 0; i < 20; i += 2 {
		sum += uint32(binary.BigEndian.Uint16(pkt[i : i+2]))
	}
	for sum > 0xffff {
		sum = (sum >> 16) + (sum & 0xffff)
	}
	binary.BigEndian.PutUint16(pkt[10:12], ^uint16(sum))

	copy(pkt[20:], payload)
	return pkt
}
