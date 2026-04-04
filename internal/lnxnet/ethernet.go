package lnxnet

import (
	"encoding/binary"
	"net"
)

const (
	etherTypeARP  = 0x0806
	etherTypeIPv4 = 0x0800
)

type ethernetFrame struct {
	DstMAC    net.HardwareAddr
	SrcMAC    net.HardwareAddr
	EtherType uint16
	Payload   []byte
}

func parseEthernet(frame []byte) *ethernetFrame {
	if len(frame) < 14 {
		return nil
	}
	return &ethernetFrame{
		DstMAC:    net.HardwareAddr(frame[0:6]),
		SrcMAC:    net.HardwareAddr(frame[6:12]),
		EtherType: binary.BigEndian.Uint16(frame[12:14]),
		Payload:   frame[14:],
	}
}

func buildEthernet(dst, src net.HardwareAddr, etherType uint16, payload []byte) []byte {
	frame := make([]byte, 14+len(payload))
	copy(frame[0:6], dst)
	copy(frame[6:12], src)
	binary.BigEndian.PutUint16(frame[12:14], etherType)
	copy(frame[14:], payload)
	return frame
}

func (b *Bridge) handleFrame(frame []byte) {
	eth := parseEthernet(frame)
	if eth == nil {
		return
	}

	// Learn the guest's MAC from the first frame we see.
	if b.guestMAC == nil {
		b.guestMAC = make(net.HardwareAddr, 6)
		copy(b.guestMAC, eth.SrcMAC)
	}

	switch eth.EtherType {
	case etherTypeARP:
		b.handleARP(eth)
	case etherTypeIPv4:
		b.handleIPv4(eth)
	}
}
