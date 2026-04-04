package lnxnet

import (
	"encoding/binary"
	"net"
)

const (
	protoICMP = 1
	protoTCP  = 6
	protoUDP  = 17
)

type ipv4Header struct {
	IHL      int
	TotalLen int
	Protocol uint8
	SrcIP    net.IP
	DstIP    net.IP
	Payload  []byte
	Raw      []byte // full IP packet including header
}

func parseIPv4(data []byte) *ipv4Header {
	if len(data) < 20 {
		return nil
	}
	ihl := int(data[0]&0x0f) * 4
	if len(data) < ihl {
		return nil
	}
	totalLen := int(binary.BigEndian.Uint16(data[2:4]))
	if totalLen > len(data) {
		totalLen = len(data)
	}

	return &ipv4Header{
		IHL:      ihl,
		TotalLen: totalLen,
		Protocol: data[9],
		SrcIP:    net.IP(data[12:16]),
		DstIP:    net.IP(data[16:20]),
		Payload:  data[ihl:totalLen],
		Raw:      data[:totalLen],
	}
}

func (b *Bridge) handleIPv4(eth *ethernetFrame) {
	ip := parseIPv4(eth.Payload)
	if ip == nil {
		return
	}

	switch ip.Protocol {
	case protoUDP:
		b.handleUDP(eth, ip)
	case protoTCP:
		b.handleTCP(eth, ip)
	case protoICMP:
		// TODO: ping support
	}
}
