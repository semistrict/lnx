package lnx

import (
	"encoding/binary"
	"encoding/gob"
	"fmt"
	"io"
	"log/slog"
	"net"
	"sync"

	"github.com/semistrict/lnx/internal/protocol"
)

// portForwarder manages automatic port forwarding from guest to host.
type portForwarder struct {
	sock VsockDevice

	mu     sync.Mutex
	auto   map[uint16]*forwardedPort // guest port -> listener
	manual map[uint16]*forwardedPort // host port -> listener
}

type forwardedPort struct {
	guestPort uint16
	hostPort  uint16
	listener  net.Listener
	done      chan struct{}
	visible   bool
}

func newPortForwarder(sock VsockDevice) *portForwarder {
	return &portForwarder{
		sock:   sock,
		auto:   make(map[uint16]*forwardedPort),
		manual: make(map[uint16]*forwardedPort),
	}
}

// run reads PortForward notifications and manages host listeners.
func (pf *portForwarder) run(conn net.Conn) {
	defer conn.Close()
	dec := gob.NewDecoder(conn)

	for {
		var msg protocol.PortForward
		if err := dec.Decode(&msg); err != nil {
			return
		}
		pf.reconcile(msg.Ports)
	}
}

// reconcile starts/stops port forwarding to match the desired set.
func (pf *portForwarder) reconcile(ports []uint16) {
	pf.mu.Lock()
	defer pf.mu.Unlock()

	want := map[uint16]bool{}
	for _, p := range ports {
		want[p] = true
	}

	// Stop forwarding ports that are no longer listening.
	for gp, fp := range pf.auto {
		if !want[gp] {
			slog.Info("port forward stop", "guest", gp, "host", fp.hostPort)
			close(fp.done)
			fp.listener.Close()
			delete(pf.auto, gp)
		}
	}

	// Start forwarding new ports.
	for _, gp := range ports {
		if _, ok := pf.findManualByGuestPortLocked(gp); ok {
			continue
		}
		if _, ok := pf.auto[gp]; ok {
			continue
		}
		fp := pf.startAutoForward(gp)
		if fp != nil {
			pf.auto[gp] = fp
		}
	}
}

// startAutoForward binds a host TCP listener and returns the forwardedPort.
// Tries the same port first, then increments if busy.
func (pf *portForwarder) startAutoForward(guestPort uint16) *forwardedPort {
	var ln net.Listener
	hostPort := guestPort

	for attempts := 0; attempts < 100; attempts++ {
		var err error
		ln, err = net.Listen("tcp", fmt.Sprintf("127.0.0.1:%d", hostPort))
		if err == nil {
			break
		}
		hostPort++
	}
	if ln == nil {
		slog.Warn("port forward failed to bind", "guest", guestPort)
		return nil
	}

	if hostPort == guestPort {
		slog.Info("port forward", "port", guestPort)
	} else {
		slog.Info("port forward", "guest", guestPort, "host", hostPort)
	}

	fp := &forwardedPort{
		guestPort: guestPort,
		hostPort:  hostPort,
		listener:  ln,
		done:      make(chan struct{}),
		visible:   true,
	}

	go pf.acceptLoop(fp)
	return fp
}

// exposeHost binds an explicit host listener for guestPort.
// If requestedHostPort is 0, an ephemeral host port is chosen.
// Returns the bound host port and whether a new mapping was created.
func (pf *portForwarder) exposeHost(guestPort, requestedHostPort uint16, visible bool) (uint16, bool, error) {
	pf.mu.Lock()
	defer pf.mu.Unlock()

	if requestedHostPort == 0 {
		if fp, ok := pf.findReusableManualByGuestPortLocked(guestPort, visible); ok {
			if visible {
				fp.visible = true
			}
			return fp.hostPort, false, nil
		}
	}

	if requestedHostPort != 0 {
		if fp, ok := pf.findByHostPortLocked(requestedHostPort); ok {
			if fp.guestPort == guestPort {
				if visible {
					fp.visible = true
				}
				return fp.hostPort, false, nil
			}
			return 0, false, fmt.Errorf("host port %d is already forwarded to guest port %d", requestedHostPort, fp.guestPort)
		}
	}

	ln, hostPort, err := bindHostPort(requestedHostPort, visible)
	if err != nil {
		if requestedHostPort == 0 {
			return 0, false, fmt.Errorf("bind ephemeral host port: %w", err)
		}
		return 0, false, fmt.Errorf("bind host port %d: %w", requestedHostPort, err)
	}

	fp := &forwardedPort{
		guestPort: guestPort,
		hostPort:  hostPort,
		listener:  ln,
		done:      make(chan struct{}),
		visible:   visible,
	}
	pf.manual[hostPort] = fp
	go pf.acceptLoop(fp)
	return hostPort, true, nil
}

func bindHostPort(port uint16, visible bool) (net.Listener, uint16, error) {
	host := "0.0.0.0"
	if visible {
		host = "127.0.0.1"
	}
	addr := net.JoinHostPort(host, "0")
	if port != 0 {
		addr = net.JoinHostPort(host, fmt.Sprintf("%d", port))
	}
	ln, err := net.Listen("tcp", addr)
	if err != nil {
		return nil, 0, err
	}
	tcpAddr, ok := ln.Addr().(*net.TCPAddr)
	if !ok {
		ln.Close()
		return nil, 0, fmt.Errorf("unexpected listener addr type %T", ln.Addr())
	}
	return ln, uint16(tcpAddr.Port), nil
}

func (pf *portForwarder) findByHostPortLocked(hostPort uint16) (*forwardedPort, bool) {
	if fp, ok := pf.manual[hostPort]; ok {
		return fp, true
	}
	for _, fp := range pf.auto {
		if fp.hostPort == hostPort {
			return fp, true
		}
	}
	return nil, false
}

func (pf *portForwarder) findManualByGuestPortLocked(guestPort uint16) (*forwardedPort, bool) {
	for _, fp := range pf.manual {
		if fp.guestPort == guestPort {
			return fp, true
		}
	}
	return nil, false
}

func (pf *portForwarder) findReusableManualByGuestPortLocked(guestPort uint16, visible bool) (*forwardedPort, bool) {
	var fallback *forwardedPort
	for _, fp := range pf.manual {
		if fp.guestPort != guestPort {
			continue
		}
		if !visible && !fp.visible {
			return fp, true
		}
		if fallback == nil {
			fallback = fp
		}
	}
	if fallback != nil {
		return fallback, true
	}
	return nil, false
}

func (pf *portForwarder) removeHost(hostPort uint16) bool {
	pf.mu.Lock()
	defer pf.mu.Unlock()

	fp, ok := pf.manual[hostPort]
	if !ok {
		return false
	}
	close(fp.done)
	fp.listener.Close()
	delete(pf.manual, hostPort)
	return true
}

func (pf *portForwarder) listVisiblePorts() []PortEntry {
	pf.mu.Lock()
	defer pf.mu.Unlock()

	var ports []PortEntry
	for _, fp := range pf.auto {
		if fp.visible {
			ports = append(ports, PortEntry{Guest: fp.guestPort, Host: fp.hostPort})
		}
	}
	for _, fp := range pf.manual {
		if fp.visible {
			ports = append(ports, PortEntry{Guest: fp.guestPort, Host: fp.hostPort})
		}
	}
	return ports
}

func (pf *portForwarder) acceptLoop(fp *forwardedPort) {
	for {
		conn, err := fp.listener.Accept()
		if err != nil {
			select {
			case <-fp.done:
				return
			default:
				slog.Info("port forward accept failed", "error", err)
				return
			}
		}
		go pf.forward(conn, fp.guestPort)
	}
}

// forward connects to the guest via vsock and splices data.
func (pf *portForwarder) forward(hostConn net.Conn, guestPort uint16) {
	defer hostConn.Close()

	// Connect to guest's port forward data listener via vsock.
	vsockConn, err := pf.sock.Connect(protocol.PortForwardDataPort)
	if err != nil {
		slog.Info("port forward vsock connect failed", "port", guestPort, "error", err)
		return
	}
	defer vsockConn.Close()

	// Send 2-byte target port header.
	var portBuf [2]byte
	binary.BigEndian.PutUint16(portBuf[:], guestPort)
	if _, err := vsockConn.Write(portBuf[:]); err != nil {
		return
	}

	// Splice bidirectionally, propagating EOF in both directions.
	done := make(chan struct{})
	go func() {
		io.Copy(hostConn, vsockConn)
		if tc, ok := hostConn.(*net.TCPConn); ok {
			tc.CloseWrite()
		}
		close(done)
	}()
	io.Copy(vsockConn, hostConn)
	vsockConn.Close()
	<-done
}

func (pf *portForwarder) close() {
	pf.mu.Lock()
	defer pf.mu.Unlock()
	for gp, fp := range pf.auto {
		close(fp.done)
		fp.listener.Close()
		delete(pf.auto, gp)
	}
	for hp, fp := range pf.manual {
		close(fp.done)
		fp.listener.Close()
		delete(pf.manual, hp)
	}
}
