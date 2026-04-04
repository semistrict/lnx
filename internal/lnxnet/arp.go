package lnxnet

import (
	"encoding/binary"
	"net"
)

const (
	arpRequest = 1
	arpReply   = 2
)

func (b *Bridge) handleARP(eth *ethernetFrame) {
	if len(eth.Payload) < 28 {
		return
	}
	p := eth.Payload

	op := binary.BigEndian.Uint16(p[6:8])
	if op != arpRequest {
		return
	}

	// Target IP the guest is asking about.
	targetIP := net.IP(p[24:28])
	senderIP := net.IP(p[14:18])
	senderMAC := net.HardwareAddr(p[8:14])

	// Only respond if they're asking for the gateway.
	if !targetIP.Equal(net.ParseIP(GatewayIP)) {
		return
	}

	// Build ARP reply.
	reply := make([]byte, 28)
	binary.BigEndian.PutUint16(reply[0:2], 1)    // hardware type: ethernet
	binary.BigEndian.PutUint16(reply[2:4], 0x0800) // protocol type: IPv4
	reply[4] = 6                                    // hardware size
	reply[5] = 4                                    // protocol size
	binary.BigEndian.PutUint16(reply[6:8], arpReply)
	copy(reply[8:14], gatewayMAC)                  // sender MAC (gateway)
	copy(reply[14:18], targetIP.To4())             // sender IP (gateway)
	copy(reply[18:24], senderMAC)                  // target MAC (guest)
	copy(reply[24:28], senderIP.To4())             // target IP (guest)

	frame := buildEthernet(senderMAC, gatewayMAC, etherTypeARP, reply)
	b.sendFrame(frame)
}
