package lnx

import (
	"net"
	"testing"
)

type stubVsockDevice struct{}

func (stubVsockDevice) Listen(port uint32) (net.Listener, error) { return nil, nil }
func (stubVsockDevice) Connect(port uint32) (net.Conn, error)    { return nil, nil }

func TestPortForwarderExposeHost(t *testing.T) {
	pf := newPortForwarder(stubVsockDevice{})
	t.Cleanup(func() { pf.close() })

	hostPort, created, err := pf.exposeHost(8080, 0, true)
	if err != nil {
		t.Fatalf("exposeHost: %v", err)
	}
	if !created {
		t.Fatal("expected new mapping to be created")
	}
	if hostPort == 0 {
		t.Fatal("expected allocated host port")
	}

	ports := pf.listVisiblePorts()
	if len(ports) != 1 || ports[0].Guest != 8080 || ports[0].Host != hostPort {
		t.Fatalf("unexpected visible ports: %+v", ports)
	}
}

func TestPortForwarderExposeHostRejectsOverlap(t *testing.T) {
	pf := newPortForwarder(stubVsockDevice{})
	t.Cleanup(func() { pf.close() })

	hostPort, _, err := pf.exposeHost(8080, 0, true)
	if err != nil {
		t.Fatalf("first exposeHost: %v", err)
	}

	if _, _, err := pf.exposeHost(9090, hostPort, true); err == nil {
		t.Fatalf("expected overlap error for host port %d", hostPort)
	}
}

func TestPortForwarderExposeHostReusesHiddenMapping(t *testing.T) {
	pf := newPortForwarder(stubVsockDevice{})
	t.Cleanup(func() { pf.close() })

	hostPort, created, err := pf.exposeHost(8080, 0, false)
	if err != nil {
		t.Fatalf("first exposeHost: %v", err)
	}
	if !created {
		t.Fatal("expected first mapping to be created")
	}

	reusedPort, created, err := pf.exposeHost(8080, 0, false)
	if err != nil {
		t.Fatalf("second exposeHost: %v", err)
	}
	if created {
		t.Fatal("expected second mapping to be reused")
	}
	if reusedPort != hostPort {
		t.Fatalf("reused host port = %d, want %d", reusedPort, hostPort)
	}
}

func TestPortForwarderReconcileSkipsManualGuestPort(t *testing.T) {
	pf := newPortForwarder(stubVsockDevice{})
	t.Cleanup(func() { pf.close() })

	if _, _, err := pf.exposeHost(8080, 0, false); err != nil {
		t.Fatalf("exposeHost: %v", err)
	}
	pf.reconcile([]uint16{8080})
	if len(pf.auto) != 0 {
		t.Fatalf("expected no auto forward for manually exposed guest port, got %+v", pf.auto)
	}
}

func TestPortForwarderExposeHostPrefersHiddenMappingForVMExpose(t *testing.T) {
	pf := newPortForwarder(stubVsockDevice{})
	t.Cleanup(func() { pf.close() })

	hiddenPort, _, err := pf.exposeHost(8080, 0, false)
	if err != nil {
		t.Fatalf("hidden exposeHost: %v", err)
	}
	if _, _, err := pf.exposeHost(8080, 9090, true); err != nil {
		t.Fatalf("visible exposeHost: %v", err)
	}

	reusedPort, created, err := pf.exposeHost(8080, 0, false)
	if err != nil {
		t.Fatalf("reuse exposeHost: %v", err)
	}
	if created {
		t.Fatal("expected hidden mapping to be reused")
	}
	if reusedPort != hiddenPort {
		t.Fatalf("reused host port = %d, want hidden port %d", reusedPort, hiddenPort)
	}
}
