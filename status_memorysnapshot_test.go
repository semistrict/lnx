package lnx

import (
	"context"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/semistrict/lnx/internal/protocol"
)

type testMemorySnapshotter struct{}

type testVsockDevice struct {
	connect func(port uint32) (net.Conn, error)
}

func (d testVsockDevice) Listen(port uint32) (net.Listener, error) { panic("unexpected Listen") }
func (d testVsockDevice) Connect(port uint32) (net.Conn, error) {
	return d.connect(port)
}

func (testMemorySnapshotter) Pause() error  { return nil }
func (testMemorySnapshotter) Resume() error { return nil }
func (testMemorySnapshotter) CreateMemorySnapshot(statePath, memPath string) error {
	if err := os.WriteFile(statePath, []byte("state"), 0644); err != nil {
		return err
	}
	return os.WriteFile(memPath, []byte("mem"), 0644)
}

func TestCreateMemorySnapshotBundleUsesConfiguredSwapPath(t *testing.T) {
	tmp := t.TempDir()
	instanceDir := filepath.Join(tmp, "instance")
	if err := os.MkdirAll(instanceDir, 0755); err != nil {
		t.Fatalf("mkdir instance dir: %v", err)
	}

	rootfsPath := filepath.Join(tmp, "rootfs.ext4")
	if err := os.WriteFile(rootfsPath, []byte("rootfs"), 0644); err != nil {
		t.Fatalf("write rootfs: %v", err)
	}

	swapDir := filepath.Join(tmp, "work")
	if err := os.MkdirAll(swapDir, 0755); err != nil {
		t.Fatalf("mkdir swap dir: %v", err)
	}
	swapPath := filepath.Join(swapDir, "swap.img")
	if err := os.WriteFile(swapPath, []byte("swapdata"), 0644); err != nil {
		t.Fatalf("write swap: %v", err)
	}

	s := newAPIServer(nil, "tester", rootfsPath, instanceDir, swapPath)
	s.snapshotter = testMemorySnapshotter{}

	bundleDir, err := s.createMemorySnapshotBundle("snap")
	if err != nil {
		t.Fatalf("createMemorySnapshotBundle: %v", err)
	}

	data, err := os.ReadFile(filepath.Join(bundleDir, "swap.img"))
	if err != nil {
		t.Fatalf("read bundled swap: %v", err)
	}
	if string(data) != "swapdata" {
		t.Fatalf("unexpected bundled swap contents: %q", string(data))
	}
}

func TestCreateMemorySnapshotBundleRejectsPathTraversalNames(t *testing.T) {
	tmp := t.TempDir()
	rootfsPath := filepath.Join(tmp, "rootfs.ext4")
	if err := os.WriteFile(rootfsPath, []byte("rootfs"), 0644); err != nil {
		t.Fatalf("write rootfs: %v", err)
	}

	s := newAPIServer(nil, "tester", rootfsPath, filepath.Join(tmp, "instance"))
	s.snapshotter = testMemorySnapshotter{}

	if _, err := s.createMemorySnapshotBundle("../../tmp/x"); err == nil {
		t.Fatal("expected invalid snapshot name error")
	}
}

func TestHandleMemorySnapshotCreatePrefersGuestProxyInWrapperMode(t *testing.T) {
	inner := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/memorysnapshot/create" {
			http.NotFound(w, r)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"path":"inner-bundle"}`))
	}))
	defer inner.Close()

	s := newAPIServer(nil, "tester", "/tmp/rootfs")
	s.snapshotter = testMemorySnapshotter{}
	s.proxyMemorySnapshotToGuest = true
	s.sock = testVsockDevice{
		connect: func(port uint32) (net.Conn, error) {
			if port != protocol.GuestHTTPPort {
				t.Fatalf("unexpected guest HTTP port %d", port)
			}
			return (&net.Dialer{}).DialContext(context.Background(), "tcp", strings.TrimPrefix(inner.URL, "http://"))
		},
	}

	req := httptest.NewRequest(http.MethodPost, "/memorysnapshot/create", nil)
	rec := httptest.NewRecorder()
	s.handleMemorySnapshotCreate(rec, req)

	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d body=%s", rec.Code, rec.Body.String())
	}
	if got := rec.Body.String(); !strings.Contains(got, "inner-bundle") {
		t.Fatalf("unexpected body %q", got)
	}
}

func TestCreateMemorySnapshotBundleUsesUniqueDefaultNames(t *testing.T) {
	tmp := t.TempDir()
	instanceDir := filepath.Join(tmp, "instance")
	if err := os.MkdirAll(instanceDir, 0755); err != nil {
		t.Fatalf("mkdir instance dir: %v", err)
	}

	rootfsPath := filepath.Join(tmp, "rootfs.ext4")
	if err := os.WriteFile(rootfsPath, []byte("rootfs"), 0644); err != nil {
		t.Fatalf("write rootfs: %v", err)
	}

	s := newAPIServer(nil, "tester", rootfsPath, instanceDir)
	s.snapshotter = testMemorySnapshotter{}

	first, err := s.createMemorySnapshotBundle("")
	if err != nil {
		t.Fatalf("first createMemorySnapshotBundle: %v", err)
	}
	second, err := s.createMemorySnapshotBundle("")
	if err != nil {
		t.Fatalf("second createMemorySnapshotBundle: %v", err)
	}
	if first == second {
		t.Fatalf("expected unique snapshot bundle dirs, got %q", first)
	}
}
