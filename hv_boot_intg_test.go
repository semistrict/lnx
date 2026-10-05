//go:build darwin && integration

package lnx

import (
	"bytes"
	"io"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/semistrict/lnx/internal/hv"
)

// TestHV_EchoHello boots Linux via Hypervisor.framework, gets a shell,
// and runs "echo hello".
func TestHV_EchoHello(t *testing.T) {
	kernelPath := filepath.Join(os.Getenv("HOME"), ".lnx", "vmlinuz")
	if _, err := os.Stat(kernelPath); err != nil {
		t.Skipf("kernel not found at %s", kernelPath)
	}

	rootfsPath := findRootfs(t)
	if rootfsPath == "" {
		t.Skip("no rootfs found")
	}

	tmpDir := t.TempDir()
	testRootfs := filepath.Join(tmpDir, "rootfs.ext4")
	if err := cloneFile(rootfsPath, testRootfs); err != nil {
		data, err2 := os.ReadFile(rootfsPath)
		if err2 != nil {
			t.Fatalf("read rootfs: %v (clone: %v)", err2, err)
		}
		if err2 := os.WriteFile(testRootfs, data, 0644); err2 != nil {
			t.Fatalf("write rootfs: %v", err2)
		}
	}

	// Capture UART output in a buffer (also tee to stderr so test -v shows it).
	var output safeBuffer
	uartW := io.MultiWriter(&output, os.Stderr)

	cfg := &Config{
		KernelPath:  kernelPath,
		RootfsPath:  testRootfs,
		CPUs:        1,
		MemoryBytes: 512 << 20,
		Backend:     "hv",
		UARTWriter:  uartW,
		KernelArgs:  "init=/bin/sh",
	}

	vm, err := buildHVVM(cfg)
	if err != nil {
		t.Fatalf("buildHVVM: %v", err)
	}
	hvvm := vm.(*hvVM)

	if err := vm.Start(); err != nil {
		t.Fatalf("Start: %v", err)
	}
	defer hvvm.Stop()

	// init=/bin/sh gives a root shell directly.
	waitForAny(t, &output, []string{"# ", "$ "}, 30*time.Second)
	t.Log("got shell prompt")

	// Send "echo hello".
	sendInput(hvvm, "echo hello\n")
	waitFor(t, &output, "hello", 5*time.Second)
	t.Log("echo hello succeeded!")
}

// TestHV_VsockDiag boots with init=/bin/sh and checks the virtio-vsock
// driver state. This helps diagnose why RX processing fails.
func TestHV_VsockDiag(t *testing.T) {
	kernelPath := filepath.Join(os.Getenv("HOME"), ".lnx", "vmlinuz")
	if _, err := os.Stat(kernelPath); err != nil {
		t.Skipf("kernel not found at %s", kernelPath)
	}
	rootfsPath := findRootfs(t)
	if rootfsPath == "" {
		t.Skip("no rootfs found")
	}

	tmpDir := t.TempDir()
	testRootfs := filepath.Join(tmpDir, "rootfs.ext4")
	if err := cloneFile(rootfsPath, testRootfs); err != nil {
		data, err2 := os.ReadFile(rootfsPath)
		if err2 != nil {
			t.Fatalf("read rootfs: %v (clone: %v)", err2, err)
		}
		if err2 := os.WriteFile(testRootfs, data, 0644); err2 != nil {
			t.Fatalf("write rootfs: %v", err2)
		}
	}

	var output safeBuffer
	uartW := io.MultiWriter(&output, os.Stderr)

	cfg := &Config{
		KernelPath:  kernelPath,
		RootfsPath:  testRootfs,
		CPUs:        1,
		MemoryBytes: 512 << 20,
		Backend:     "hv",
		UARTWriter:  uartW,
		KernelArgs:  "init=/bin/sh",
	}

	vm, err := buildHVVM(cfg)
	if err != nil {
		t.Fatalf("buildHVVM: %v", err)
	}
	hvvm := vm.(*hvVM)
	if err := vm.Start(); err != nil {
		t.Fatalf("Start: %v", err)
	}
	defer hvvm.Stop()

	waitForAny(t, &output, []string{"# ", "$ "}, 30*time.Second)

	// Mount proc and sys so we can inspect.
	sendInput(hvvm, "mount -t proc proc /proc 2>/dev/null; mount -t sysfs sys /sys 2>/dev/null\n")
	time.Sleep(200 * time.Millisecond)

	// Check virtio device binding.
	sendInput(hvvm, "ls /sys/bus/virtio/devices/\n")
	waitFor(t, &output, "virtio", 5*time.Second)

	// Check vsock driver binding.
	sendInput(hvvm, "ls -la /sys/bus/virtio/drivers/\n")
	time.Sleep(500 * time.Millisecond)

	// Check interrupts.
	sendInput(hvvm, "cat /proc/interrupts\n")
	time.Sleep(500 * time.Millisecond)

	// Check dmesg for vsock.
	sendInput(hvvm, "dmesg | grep -i vsock\n")
	time.Sleep(500 * time.Millisecond)

	// Check vsock device features and driver binding.
	sendInput(hvvm, "cat /sys/bus/virtio/devices/virtio0/device\n")
	time.Sleep(200 * time.Millisecond)
	sendInput(hvvm, "cat /sys/bus/virtio/devices/virtio0/status\n")
	time.Sleep(200 * time.Millisecond)
	sendInput(hvvm, "readlink /sys/bus/virtio/devices/virtio0/driver 2>/dev/null || echo NO_DRIVER\n")
	time.Sleep(200 * time.Millisecond)
	// Test 1: Guest connects to host (like vsock.Dial)
	vsockLn, vsockErr := hvvm.vsock.Listen(9999)
	if vsockErr != nil {
		t.Fatalf("vsock listen: %v", vsockErr)
	}
	defer vsockLn.Close()
	go func() {
		c, e := vsockLn.Accept()
		if e != nil {
			return
		}
		c.Write([]byte("HELLO_FROM_HOST"))
		c.Close()
	}()
	sendInput(hvvm, "python3 -c \"import socket; s=socket.socket(socket.AF_VSOCK,socket.SOCK_STREAM); s.settimeout(3); s.connect((2,9999)); print(s.recv(100)); s.close()\"\n")
	// Wait for either success or timeout (python3 has 3s timeout).
	waitForAny(t, &output, []string{"HELLO_FROM_HOST", "TimeoutError", "ConnectionRefused"}, 10*time.Second)
	if !strings.Contains(output.String(), "HELLO_FROM_HOST") {
		t.Fatalf("Guest-to-host vsock FAILED, output:\n%s", output.String())
	}
	t.Log("Guest-to-host vsock: OK")

	// Test 2: Host connects to guest (like connectExec)
	// Have the guest listen, then the host connects.
	sendInput(hvvm, "python3 -c \"import socket; s=socket.socket(socket.AF_VSOCK,socket.SOCK_STREAM); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1); s.bind((socket.VMADDR_CID_ANY,8888)); s.listen(1); c,_=s.accept(); c.sendall(b'HELLO_FROM_GUEST'); c.close(); s.close(); print('H2G_DONE')\" &\n")
	time.Sleep(1000 * time.Millisecond)

	// Host connects to guest port 8888.
	guestConn, err := hvvm.vsock.Connect(8888)
	if err != nil {
		t.Fatalf("host connect to guest failed: %v", err)
	}
	buf := make([]byte, 100)
	n, _ := guestConn.Read(buf)
	guestConn.Close()
	t.Logf("Host→Guest vsock: received %q", string(buf[:n]))

	waitFor(t, &output, "H2G_DONE", 5*time.Second)
	t.Log("Host→Guest vsock: OK")

	// Check interrupts AFTER vsock traffic.
	sendInput(hvvm, "cat /proc/interrupts | grep virtio\n")
	time.Sleep(500 * time.Millisecond)

	// Read GIC distributor ISENABLER1 (SPIs 32-63) to check if SPI 50 is enabled.
	isenabler1, err := hv.GICDistRead(0x104) // GICD_ISENABLER1
	if err != nil {
		t.Logf("GICD_ISENABLER1 read failed: %v", err)
	} else {
		spi50Enabled := (isenabler1 >> 18) & 1 // SPI 50 = bit 18
		spi48Enabled := (isenabler1 >> 16) & 1 // SPI 48 = bit 16
		spi49Enabled := (isenabler1 >> 17) & 1 // SPI 49 = bit 17
		t.Logf("GICD_ISENABLER1=0x%x  SPI48(blk)=%d SPI49(net)=%d SPI50(vsock)=%d",
			isenabler1, spi48Enabled, spi49Enabled, spi50Enabled)
	}

	// Read GICD_ISPENDR1 to check pending state.
	ispendr1, err := hv.GICDistRead(0x204) // GICD_ISPENDR1
	if err != nil {
		t.Logf("GICD_ISPENDR1 read failed: %v", err)
	} else {
		t.Logf("GICD_ISPENDR1=0x%x  SPI48=%d SPI49=%d SPI50=%d",
			ispendr1, (ispendr1>>16)&1, (ispendr1>>17)&1, (ispendr1>>18)&1)
	}

	// Directly test: assert SPI 50, then check if it shows as pending.
	t.Logf("Asserting SPI 50 via GICSetSPI...")
	if err := hv.GICSetSPI(50, true); err != nil {
		t.Logf("GICSetSPI(50, true) FAILED: %v", err)
	}
	ispendr1After, err := hv.GICDistRead(0x204)
	if err != nil {
		t.Logf("GICD_ISPENDR1 read after assert failed: %v", err)
	} else {
		t.Logf("After GICSetSPI(50,true): GICD_ISPENDR1=0x%x  SPI50=%d",
			ispendr1After, (ispendr1After>>18)&1)
	}
	// Deassert to clean up.
	hv.GICSetSPI(50, false)

	// Log the output for inspection.
	t.Logf("Guest diagnostics:\n%s", output.String())
}

// TestHV_CurlGoogle boots Linux, configures networking via the lnxnet
// userspace NAT bridge, and runs curl google.com.
func TestHV_CurlGoogle(t *testing.T) {
	kernelPath := filepath.Join(os.Getenv("HOME"), ".lnx", "vmlinuz")
	if _, err := os.Stat(kernelPath); err != nil {
		t.Skipf("kernel not found at %s", kernelPath)
	}
	rootfsPath := findRootfs(t)
	if rootfsPath == "" {
		t.Skip("no rootfs found")
	}

	tmpDir := t.TempDir()
	testRootfs := filepath.Join(tmpDir, "rootfs.ext4")
	if err := cloneFile(rootfsPath, testRootfs); err != nil {
		data, err2 := os.ReadFile(rootfsPath)
		if err2 != nil {
			t.Fatalf("read rootfs: %v (clone: %v)", err2, err)
		}
		if err2 := os.WriteFile(testRootfs, data, 0644); err2 != nil {
			t.Fatalf("write rootfs: %v", err2)
		}
	}

	var output safeBuffer
	uartW := io.MultiWriter(&output, os.Stderr)

	cfg := &Config{
		KernelPath:  kernelPath,
		RootfsPath:  testRootfs,
		CPUs:        1,
		MemoryBytes: 512 << 20,
		Backend:     "hv",
		UARTWriter:  uartW,
		KernelArgs:  "init=/bin/sh",
	}

	vm, err := buildHVVM(cfg)
	if err != nil {
		t.Fatalf("buildHVVM: %v", err)
	}
	hvvm := vm.(*hvVM)

	if err := vm.Start(); err != nil {
		t.Fatalf("Start: %v", err)
	}
	defer hvvm.Stop()

	// Wait for shell prompt (init=/bin/sh gives root shell directly).
	waitForAny(t, &output, []string{"# ", "$ "}, 30*time.Second)

	// Configure networking. The lnxnet bridge provides:
	//   gateway: 192.168.64.1, guest: 192.168.64.2/24
	sendInput(hvvm, "ip link set eth0 up\n")
	time.Sleep(100 * time.Millisecond)
	sendInput(hvvm, "ip addr add 192.168.64.2/24 dev eth0\n")
	time.Sleep(100 * time.Millisecond)
	sendInput(hvvm, "ip route add default via 192.168.64.1\n")
	time.Sleep(100 * time.Millisecond)
	sendInput(hvvm, "echo 'nameserver 8.8.8.8' > /etc/resolv.conf\n")
	time.Sleep(100 * time.Millisecond)

	// Test connectivity.
	sendInput(hvvm, "curl -s -o /dev/null -w '%{http_code}' http://google.com\n")

	// Wait for HTTP status code 200 or 301 (google redirects to https).
	waitForAny(t, &output, []string{"200", "301"}, 15*time.Second)
	t.Log("curl google.com succeeded!")
}

func waitForAny(t *testing.T, buf *safeBuffer, substrs []string, timeout time.Duration) {
	t.Helper()
	deadline := time.After(timeout)
	for {
		select {
		case <-deadline:
			t.Fatalf("timed out waiting for any of %v.\nOutput:\n%s", substrs, buf.String())
		case <-time.After(50 * time.Millisecond):
			s := buf.String()
			for _, sub := range substrs {
				if strings.Contains(s, sub) {
					return
				}
			}
		}
	}
}

func sendInput(vm *hvVM, s string) {
	vm.uart.QueueInput([]byte(s))
	if vm.console != nil {
		vm.console.QueueInput([]byte(s))
	}
	vm.mu.Lock()
	if len(vm.vcpus) > 0 {
		vm.vcpus[0].ForceExit()
	}
	vm.mu.Unlock()
}

func waitFor(t *testing.T, buf *safeBuffer, substr string, timeout time.Duration) {
	t.Helper()
	deadline := time.After(timeout)
	for {
		select {
		case <-deadline:
			t.Fatalf("timed out waiting for %q.\nOutput:\n%s", substr, buf.String())
		case <-time.After(50 * time.Millisecond):
			if strings.Contains(buf.String(), substr) {
				return
			}
		}
	}
}

func findRootfs(t *testing.T) string {
	t.Helper()
	home := os.Getenv("HOME")
	candidates := []string{
		filepath.Join(home, ".lnx", "instances", "default", "rootfs.ext4"),
	}
	cacheDir := filepath.Join(home, ".lnx", "packed-cache")
	entries, _ := os.ReadDir(cacheDir)
	for _, e := range entries {
		if e.IsDir() {
			candidates = append(candidates, filepath.Join(cacheDir, e.Name(), "rootfs.ext4"))
		}
	}
	for _, p := range candidates {
		if _, err := os.Stat(p); err == nil {
			return p
		}
	}
	return ""
}

// safeBuffer is a bytes.Buffer safe for concurrent reads and writes.
type safeBuffer struct {
	mu  sync.Mutex
	buf bytes.Buffer
}

func (b *safeBuffer) Write(p []byte) (int, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.buf.Write(p)
}

func (b *safeBuffer) String() string {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.buf.String()
}
