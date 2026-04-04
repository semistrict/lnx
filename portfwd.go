package lnx

import (
	"encoding/binary"
	"encoding/gob"
	"fmt"
	"io"
	"log/slog"
	"net"
	"sync"

	vz "github.com/Code-Hex/vz/v3"

	"github.com/semistrict/lnx/internal/protocol"
)

// portForwarder manages automatic port forwarding from guest to host.
type portForwarder struct {
	sock *vz.VirtioSocketDevice

	mu        sync.Mutex
	listeners map[uint16]*forwardedPort // guest port -> listener
}

type forwardedPort struct {
	guestPort uint16
	hostPort  uint16
	listener  net.Listener
	done      chan struct{}
}

func newPortForwarder(sock *vz.VirtioSocketDevice) *portForwarder {
	return &portForwarder{
		sock:      sock,
		listeners: make(map[uint16]*forwardedPort),
	}
}

// run reads PortForward notifications and manages host listeners.
func (pf *portForwarder) run(conn net.Conn) {
	defer conn.Close()
	dec := gob.NewDecoder(conn)

	for {
		var msg protocol.PortForward
		if err := dec.Decode(&msg); err != nil {
			slog.Debug("port forward decode failed", "error", err)
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
	for gp, fp := range pf.listeners {
		if !want[gp] {
			slog.Info("port forward stop", "guest", gp, "host", fp.hostPort)
			close(fp.done)
			fp.listener.Close()
			delete(pf.listeners, gp)
		}
	}

	// Start forwarding new ports.
	for _, gp := range ports {
		if _, ok := pf.listeners[gp]; ok {
			continue
		}
		fp := pf.startForward(gp)
		if fp != nil {
			pf.listeners[gp] = fp
		}
	}
}

// startForward binds a host TCP listener and returns the forwardedPort.
// Tries the same port first, then increments if busy.
func (pf *portForwarder) startForward(guestPort uint16) *forwardedPort {
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
	}

	go pf.acceptLoop(fp)
	return fp
}

func (pf *portForwarder) acceptLoop(fp *forwardedPort) {
	for {
		conn, err := fp.listener.Accept()
		if err != nil {
			select {
			case <-fp.done:
				return
			default:
				slog.Debug("port forward accept failed", "error", err)
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
		slog.Debug("port forward vsock connect failed", "port", guestPort, "error", err)
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
	for gp, fp := range pf.listeners {
		close(fp.done)
		fp.listener.Close()
		delete(pf.listeners, gp)
	}
}
