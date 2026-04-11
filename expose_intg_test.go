//go:build darwin && integration

package lnx_test

import (
	"bufio"
	"bytes"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"golang.org/x/sys/unix"
)

func TestCLI_Expose_Host(t *testing.T) {
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	srcInst := fmt.Sprintf("test-expose-host-src-%d", time.Now().UnixNano())
	createClonedInstance(t, srcInst)
	registerInstanceStopCleanup(t, bin, srcInst)

	sourcePort := 18180
	hostPort := 18181

	srcCmd, srcLines, srcStderr, srcDone := startTCPServerInstance(t, bin, srcInst, sourcePort, "HELLO_HOST_EXPOSE")
	waitForCLIOutput(t, srcLines, "READY", 20*time.Second, srcStderr)
	t.Cleanup(func() { cleanupStreamingCLI(t, srcCmd, srcDone, srcStderr) })

	out := runCLISuccess(t, bin, "expose", fmt.Sprintf("%s:%d", srcInst, sourcePort), fmt.Sprintf("--as=:%d", hostPort))
	assert.Contains(t, out, fmt.Sprintf("localhost:%d -> %s:%d", hostPort, srcInst, sourcePort))

	data := readTCPEventually(t, fmt.Sprintf("127.0.0.1:%d", hostPort), "HELLO_HOST_EXPOSE", 10*time.Second)
	assert.Contains(t, data, "HELLO_HOST_EXPOSE")

	portsOut := runCLISuccess(t, bin, "--instance", srcInst, "ports", "list")
	assert.Contains(t, portsOut, fmt.Sprintf("%d", sourcePort))
	assert.Contains(t, portsOut, fmt.Sprintf("%d", hostPort))

	waitForProcessSuccess(t, srcDone, 15*time.Second, srcStderr.String())
}

func TestCLI_Expose_VMToVM(t *testing.T) {
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	srcInst := fmt.Sprintf("test-expose-src-%d", time.Now().UnixNano())
	dstInst := fmt.Sprintf("test-expose-dst-%d", time.Now().UnixNano())
	createClonedInstance(t, srcInst)
	createClonedInstance(t, dstInst)
	registerInstanceStopCleanup(t, bin, srcInst, dstInst)

	sourcePort := 18080
	destPort := 18081
	srcCmd, srcLines, srcStderr, srcDone := startTCPServerInstance(t, bin, srcInst, sourcePort, "HELLO_EXPOSE")
	waitForCLIOutput(t, srcLines, "READY", 20*time.Second, srcStderr)
	t.Cleanup(func() { cleanupStreamingCLI(t, srcCmd, srcDone, srcStderr) })

	dstCmd, dstStderr, dstDone := startIdleInstance(t, bin, dstInst)
	t.Cleanup(func() { cleanupStreamingCLI(t, dstCmd, dstDone, dstStderr) })

	exposeOut := runCLISuccess(t, bin, "expose", fmt.Sprintf("%s:%d", srcInst, sourcePort), fmt.Sprintf("--as=%s:%d", dstInst, destPort))
	assert.Contains(t, exposeOut, fmt.Sprintf("%s:%d -> %s:%d", dstInst, destPort, srcInst, sourcePort))

	clientScript := fmt.Sprintf(`python3 -c "
import socket
s = socket.create_connection(('127.0.0.1', %d), timeout=10)
print(s.recv(1024).decode(), end='')
s.close()
"`, destPort)
	clientCmd := exec.Command(bin, "--instance", dstInst, "sh", "-c", clientScript)
	clientOut, err := clientCmd.CombinedOutput()
	require.NoError(t, err, "destination client failed: %s", clientOut)
	assert.Contains(t, string(clientOut), "HELLO_EXPOSE")

	waitForProcessSuccess(t, srcDone, 15*time.Second, srcStderr.String())
}

func TestCLI_Expose_VMToVM_Idempotent(t *testing.T) {
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	srcInst := fmt.Sprintf("test-expose-idem-src-%d", time.Now().UnixNano())
	dstInst := fmt.Sprintf("test-expose-idem-dst-%d", time.Now().UnixNano())
	createClonedInstance(t, srcInst)
	createClonedInstance(t, dstInst)
	registerInstanceStopCleanup(t, bin, srcInst, dstInst)

	sourcePort := 18190
	destPort := 18191

	srcCmd, srcLines, srcStderr, srcDone := startTCPServerInstance(t, bin, srcInst, sourcePort, "HELLO_IDEMPOTENT")
	waitForCLIOutput(t, srcLines, "READY", 20*time.Second, srcStderr)
	t.Cleanup(func() { cleanupStreamingCLI(t, srcCmd, srcDone, srcStderr) })

	dstCmd, dstStderr, dstDone := startIdleInstance(t, bin, dstInst)
	t.Cleanup(func() { cleanupStreamingCLI(t, dstCmd, dstDone, dstStderr) })

	runCLISuccess(t, bin, "expose", fmt.Sprintf("%s:%d", srcInst, sourcePort), fmt.Sprintf("--as=%s:%d", dstInst, destPort))
	runCLISuccess(t, bin, "expose", fmt.Sprintf("%s:%d", srcInst, sourcePort), fmt.Sprintf("--as=%s:%d", dstInst, destPort))

	clientScript := fmt.Sprintf(`python3 -c "
import socket
s = socket.create_connection(('127.0.0.1', %d), timeout=10)
print(s.recv(1024).decode(), end='')
s.close()
"`, destPort)
	clientOut := runCLISuccess(t, bin, "--instance", dstInst, "sh", "-c", clientScript)
	assert.Contains(t, clientOut, "HELLO_IDEMPOTENT")
	waitForProcessSuccess(t, srcDone, 15*time.Second, srcStderr.String())
}

func TestCLI_Expose_RollbackOnDestinationFailure(t *testing.T) {
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	srcInst := fmt.Sprintf("test-expose-rollback-src-%d", time.Now().UnixNano())
	badDstInst := fmt.Sprintf("test-expose-missing-dst-%d", time.Now().UnixNano())
	createClonedInstance(t, srcInst)
	registerInstanceStopCleanup(t, bin, srcInst)

	srcCmd, srcStderr, srcDone := startTimedInstance(t, bin, srcInst, 2*time.Second)
	t.Cleanup(func() { cleanupStreamingCLI(t, srcCmd, srcDone, srcStderr) })
	errOut, err := runCLI(bin, "expose", fmt.Sprintf("%s:%d", srcInst, 18200), fmt.Sprintf("--as=%s:%d", badDstInst, 18201))
	require.Error(t, err)
	assert.Contains(t, errOut, fmt.Sprintf("no VM running for instance %q", badDstInst))

	waitForProcessSuccess(t, srcDone, 10*time.Second, srcStderr.String())
	waitForNoVMRunning(t, bin, srcInst, 12*time.Second)
}

func TestCLI_Expose_PortsListVisibility(t *testing.T) {
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	hostSrc := fmt.Sprintf("test-expose-visible-src-%d", time.Now().UnixNano())
	createClonedInstance(t, hostSrc)
	registerInstanceStopCleanup(t, bin, hostSrc)
	hostCmd, hostStderr, hostDone := startIdleInstance(t, bin, hostSrc)
	t.Cleanup(func() { cleanupStreamingCLI(t, hostCmd, hostDone, hostStderr) })

	runCLISuccess(t, bin, "expose", fmt.Sprintf("%s:%d", hostSrc, 18210), "--as=:18211")
	visibleOut := runCLISuccess(t, bin, "--instance", hostSrc, "ports", "list")
	assert.Contains(t, visibleOut, "18210")
	assert.Contains(t, visibleOut, "18211")

	vmSrc := fmt.Sprintf("test-expose-hidden-src-%d", time.Now().UnixNano())
	vmDst := fmt.Sprintf("test-expose-hidden-dst-%d", time.Now().UnixNano())
	createClonedInstance(t, vmSrc)
	createClonedInstance(t, vmDst)
	registerInstanceStopCleanup(t, bin, vmSrc, vmDst)
	vmSrcCmd, vmSrcStderr, vmSrcDone := startIdleInstance(t, bin, vmSrc)
	vmDstCmd, vmDstStderr, vmDstDone := startIdleInstance(t, bin, vmDst)
	t.Cleanup(func() { cleanupStreamingCLI(t, vmSrcCmd, vmSrcDone, vmSrcStderr) })
	t.Cleanup(func() { cleanupStreamingCLI(t, vmDstCmd, vmDstDone, vmDstStderr) })

	runCLISuccess(t, bin, "expose", fmt.Sprintf("%s:%d", vmSrc, 18220), fmt.Sprintf("--as=%s:%d", vmDst, 18221))
	assert.Contains(t, runCLISuccess(t, bin, "--instance", vmSrc, "ports", "list"), "no forwarded ports")
	assert.Contains(t, runCLISuccess(t, bin, "--instance", vmDst, "ports", "list"), "no forwarded ports")
}

func TestCLI_Expose_HostConflict(t *testing.T) {
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	src1 := fmt.Sprintf("test-expose-conflict-src1-%d", time.Now().UnixNano())
	src2 := fmt.Sprintf("test-expose-conflict-src2-%d", time.Now().UnixNano())
	createClonedInstance(t, src1)
	createClonedInstance(t, src2)
	registerInstanceStopCleanup(t, bin, src1, src2)
	cmd1, stderr1, done1 := startIdleInstance(t, bin, src1)
	cmd2, stderr2, done2 := startIdleInstance(t, bin, src2)
	t.Cleanup(func() { cleanupStreamingCLI(t, cmd1, done1, stderr1) })
	t.Cleanup(func() { cleanupStreamingCLI(t, cmd2, done2, stderr2) })

	runCLISuccess(t, bin, "expose", fmt.Sprintf("%s:%d", src1, 18230), "--as=:18231")
	errOut, err := runCLI(bin, "expose", fmt.Sprintf("%s:%d", src2, 18232), "--as=:18231")
	require.Error(t, err)
	assert.Contains(t, errOut, "bind host port 18231")
}

func TestCLI_Expose_DestinationReuseAndConflict(t *testing.T) {
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	src1 := fmt.Sprintf("test-expose-reuse-src1-%d", time.Now().UnixNano())
	src2 := fmt.Sprintf("test-expose-reuse-src2-%d", time.Now().UnixNano())
	dst := fmt.Sprintf("test-expose-reuse-dst-%d", time.Now().UnixNano())
	createClonedInstance(t, src1)
	createClonedInstance(t, src2)
	createClonedInstance(t, dst)
	registerInstanceStopCleanup(t, bin, src1, src2, dst)
	cmd1, stderr1, done1 := startIdleInstance(t, bin, src1)
	cmd2, stderr2, done2 := startIdleInstance(t, bin, src2)
	cmd3, stderr3, done3 := startIdleInstance(t, bin, dst)
	t.Cleanup(func() { cleanupStreamingCLI(t, cmd1, done1, stderr1) })
	t.Cleanup(func() { cleanupStreamingCLI(t, cmd2, done2, stderr2) })
	t.Cleanup(func() { cleanupStreamingCLI(t, cmd3, done3, stderr3) })

	runCLISuccess(t, bin, "expose", fmt.Sprintf("%s:%d", src1, 18240), fmt.Sprintf("--as=%s:%d", dst, 18241))
	runCLISuccess(t, bin, "expose", fmt.Sprintf("%s:%d", src1, 18240), fmt.Sprintf("--as=%s:%d", dst, 18241))

	errOut, err := runCLI(bin, "expose", fmt.Sprintf("%s:%d", src2, 18242), fmt.Sprintf("--as=%s:%d", dst, 18241))
	require.Error(t, err)
	assert.Contains(t, errOut, "port 18241 is already exposed")
}

func createClonedInstance(t *testing.T, name string) {
	t.Helper()

	home, _ := os.UserHomeDir()
	base := filepath.Join(home, ".lnx")
	instDir := filepath.Join(base, "instances", name)
	defaultRootfs := filepath.Join(base, "instances", "default", "rootfs.ext4")

	if _, err := os.Stat(defaultRootfs); err != nil {
		t.Skipf("skipping: default instance rootfs not found (run 'lnx init' first)")
	}

	require.NoError(t, os.MkdirAll(instDir, 0755))
	rootfs := filepath.Join(instDir, "rootfs.ext4")
	_ = os.Remove(rootfs)
	require.NoError(t, unix.Clonefile(defaultRootfs, rootfs, 0))
	t.Cleanup(func() { _ = os.RemoveAll(instDir) })
}

func registerInstanceStopCleanup(t *testing.T, bin string, names ...string) {
	t.Helper()
	t.Cleanup(func() {
		for _, name := range names {
			cmd := exec.Command(bin, "--instance", name, "stop")
			out, err := cmd.CombinedOutput()
			if err != nil && !strings.Contains(string(out), "no VM running") {
				t.Logf("stop %s failed: %v: %s", name, err, out)
			}
		}
	})
}

func startIdleInstance(t *testing.T, bin, instance string) (*exec.Cmd, *bytes.Buffer, <-chan error) {
	t.Helper()
	cmd, lines, stderr, done := startStreamingCLI(t, bin, "--instance", instance, "sh", "-c", "echo READY; sleep 120")
	waitForCLIOutput(t, lines, "READY", 20*time.Second, stderr)
	return cmd, stderr, done
}

func startTimedInstance(t *testing.T, bin, instance string, duration time.Duration) (*exec.Cmd, *bytes.Buffer, <-chan error) {
	t.Helper()
	script := fmt.Sprintf("echo READY; sleep %.0f", duration.Seconds())
	cmd, lines, stderr, done := startStreamingCLI(t, bin, "--instance", instance, "sh", "-c", script)
	waitForCLIOutput(t, lines, "READY", 20*time.Second, stderr)
	return cmd, stderr, done
}

func startTCPServerInstance(t *testing.T, bin, instance string, port int, payload string) (*exec.Cmd, <-chan string, *bytes.Buffer, <-chan error) {
	t.Helper()
	script := fmt.Sprintf(`python3 -c "
import socket, time
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('0.0.0.0', %d))
s.listen(1)
print('READY', flush=True)
conn, _ = s.accept()
conn.sendall(b'%s\n')
conn.close()
s.close()
time.sleep(1)
"`, port, payload)
	return startStreamingCLI(t, bin, "--instance", instance, "sh", "-c", script)
}

func runCLI(bin string, args ...string) (string, error) {
	cmd := exec.Command(bin, args...)
	out, err := cmd.CombinedOutput()
	return string(out), err
}

func runCLISuccess(t *testing.T, bin string, args ...string) string {
	t.Helper()
	out, err := runCLI(bin, args...)
	require.NoError(t, err, "command failed: %s %v\n%s", bin, args, out)
	return out
}

func readTCPOnce(t *testing.T, addr string) string {
	t.Helper()
	conn, err := net.DialTimeout("tcp", addr, 10*time.Second)
	require.NoError(t, err)
	defer conn.Close()
	data, err := io.ReadAll(conn)
	require.NoError(t, err)
	return string(data)
}

func readTCPEventually(t *testing.T, addr, want string, timeout time.Duration) string {
	t.Helper()
	deadline := time.Now().Add(timeout)
	var last string
	for time.Now().Before(deadline) {
		conn, err := net.DialTimeout("tcp", addr, time.Second)
		if err == nil {
			data, readErr := io.ReadAll(conn)
			_ = conn.Close()
			if readErr == nil {
				last = string(data)
				if strings.Contains(last, want) {
					return last
				}
			}
		}
		time.Sleep(500 * time.Millisecond)
	}
	t.Fatalf("timed out waiting for %q from %s; last response=%q", want, addr, last)
	return ""
}

func startStreamingCLI(t *testing.T, bin string, args ...string) (*exec.Cmd, <-chan string, *bytes.Buffer, <-chan error) {
	t.Helper()

	cmd := exec.Command(bin, args...)
	stdout, err := cmd.StdoutPipe()
	require.NoError(t, err)
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	require.NoError(t, cmd.Start())

	lines := make(chan string, 32)
	go func() {
		defer close(lines)
		scanner := bufio.NewScanner(stdout)
		scanner.Buffer(make([]byte, 0, 1024), 1024*1024)
		for scanner.Scan() {
			lines <- scanner.Text()
		}
	}()

	done := make(chan error, 1)
	go func() { done <- cmd.Wait() }()
	return cmd, lines, &stderr, done
}

func waitForCLIOutput(t *testing.T, lines <-chan string, want string, timeout time.Duration, stderr *bytes.Buffer) {
	t.Helper()

	deadline := time.After(timeout)
	for {
		select {
		case line, ok := <-lines:
			if !ok {
				t.Fatalf("process exited before output %q appeared; stderr: %s", want, stderr.String())
			}
			if strings.Contains(line, want) {
				return
			}
		case <-deadline:
			t.Fatalf("timed out waiting for %q; stderr: %s", want, stderr.String())
		}
	}
}

func cleanupStreamingCLI(t *testing.T, cmd *exec.Cmd, done <-chan error, stderr *bytes.Buffer) {
	t.Helper()

	select {
	case <-done:
		return
	default:
	}

	if cmd.Process != nil {
		_ = cmd.Process.Kill()
	}
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Logf("timed out waiting for process cleanup; stderr: %s", stderr.String())
	}
}

func waitForProcessSuccess(t *testing.T, done <-chan error, timeout time.Duration, stderr string) {
	t.Helper()
	select {
	case err := <-done:
		require.NoError(t, err, "process failed: %s", stderr)
	case <-time.After(timeout):
		t.Fatalf("process did not exit within %s; stderr: %s", timeout, stderr)
	}
}

func waitForNoVMRunning(t *testing.T, bin, instance string, timeout time.Duration) {
	t.Helper()
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		out, err := runCLI(bin, "--instance", instance, "status")
		if err == nil && strings.Contains(out, "no VM running") {
			return
		}
		time.Sleep(500 * time.Millisecond)
	}
	out, _ := runCLI(bin, "--instance", instance, "status")
	t.Fatalf("VM %s still running after %s: %s", instance, timeout, out)
}
