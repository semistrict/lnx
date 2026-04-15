//go:build darwin && integration

package lnx_test

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"math/rand"
	"net"
	"os"
	"net/http"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/semistrict/lnx"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestQEMU_ForkPreservesMemory(t *testing.T) {
	if parseQemuBackendForTest() == "" {
		t.Skip("requires LNX_BACKEND=qemu:...")
	}

	t0 := time.Now()
	origDir := setupQemuTestDir(t)
	origCfg := testConfig(origDir)

	// Boot original VM as a daemon.
	go lnx.RunDaemon(origCfg)

	origClient := apiClient(origDir)
	waitForExec(t, origClient, 60*time.Second)
	t.Logf("[%dms] original VM ready", time.Since(t0).Milliseconds())

	// Verify exec works.
	out := execCommand(t, origClient, "echo", "test123")
	require.Equal(t, "test123", out, "basic exec should work")

	// Start a background counter at a random offset.
	startVal := 40000 + rand.Intn(10000)
	counterCmd := fmt.Sprintf("i=%d; while true; do echo $i > /tmp/count.txt; i=$((i+1)); sleep 0.1; done", startVal)
	execCommand(t, origClient, "sh", "-c", counterCmd+" < /dev/null > /dev/null 2>&1 &")
	time.Sleep(500 * time.Millisecond)

	// Read the counter from the original.
	origVal := readCounter(t, origClient)
	t.Logf("[%dms] original counter: %d (started at %d)", time.Since(t0).Milliseconds(), origVal, startVal)
	require.Greater(t, origVal, startVal, "counter should have advanced")

	// Fork the running VM. Must be on same APFS volume for clonefile.
	forkDir, err := os.MkdirTemp(filepath.Dir(origDir), "fork-*")
	require.NoError(t, err)
	t.Cleanup(func() { os.RemoveAll(forkDir) })
	qmpSock := filepath.Join(origDir, "qmp.sock")
	exited, err := lnx.ForkQemuVM(qmpSock, origDir, forkDir)
	require.NoError(t, err, "ForkQemuVM")
	if !exited {
		require.NoError(t, lnx.QMPResume(qmpSock), "resume original VM")
	}
	t.Logf("[%dms] fork snapshot done (exited=%v)", time.Since(t0).Milliseconds(), exited)

	// Boot the fork.
	forkCfg := testConfig(forkDir)
	go lnx.RunDaemon(forkCfg)

	forkClient := apiClient(forkDir)
	waitForExec(t, forkClient, 60*time.Second)
	t.Logf("[%dms] fork VM ready", time.Since(t0).Milliseconds())

	// Check fork state.
	forkEcho := execCommand(t, forkClient, "echo", "fork-alive")
	t.Logf("fork echo: %q", forkEcho)

	// The fork's counter should be at or above where the original was.
	forkVal := readCounter(t, forkClient)
	t.Logf("[%dms] fork counter: %d (original was %d)", time.Since(t0).Milliseconds(), forkVal, origVal)
	assert.GreaterOrEqual(t, forkVal, origVal,
		"fork counter should be >= original's value at fork time")

	// Wait and verify the counter is still incrementing in the fork.
	time.Sleep(500 * time.Millisecond)
	forkVal2 := readCounter(t, forkClient)
	t.Logf("[%dms] fork counter after 500ms: %d", time.Since(t0).Milliseconds(), forkVal2)
	assert.Greater(t, forkVal2, forkVal, "fork counter should still be incrementing")
}

func TestQEMU_Fork(t *testing.T) {
	if parseQemuBackendForTest() == "" {
		t.Skip("requires LNX_BACKEND=qemu:...")
	}
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := "test-qemu-fork"
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	cmd, stderr, done := startTimedInstance(t, bin, inst, 60*time.Second)
	t.Cleanup(func() { cleanupStreamingCLI(t, cmd, done, stderr) })

	// Start HTTP server with state.
	runCLISuccess(t, bin, "--instance", inst, "sh", "-c",
		`cat > /tmp/server.py << 'PYEOF'
import os, resource
for fd in range(3, min(resource.getrlimit(resource.RLIMIT_NOFILE)[0], 1024)):
    try: os.close(fd)
    except: pass
os.setsid()
from http.server import HTTPServer, BaseHTTPRequestHandler
class H(BaseHTTPRequestHandler):
    state = {"value": "original"}
    def do_GET(self):
        self.send_response(200); self.end_headers()
        self.wfile.write(f'{H.state["value"]}\n'.encode())
    def log_message(self, *a): pass
HTTPServer(("0.0.0.0", 8888), H).serve_forever()
PYEOF
python3 -u /tmp/server.py </dev/null >/dev/null 2>&1 &`)

	require.Eventually(t, func() bool {
		out, err := runCLI(bin, "--instance", inst, "curl", "-sf", "http://127.0.0.1:8888/")
		return err == nil && strings.TrimSpace(out) == "original"
	}, 30*time.Second, time.Second, "HTTP server never became ready")

	// Fork.
	forkOut := runCLISuccess(t, bin, "--instance", inst, "fork")
	assert.Contains(t, forkOut, "forked to")
	childInst := strings.TrimSpace(strings.TrimPrefix(strings.TrimSpace(forkOut), "forked to"))
	require.NotEmpty(t, childInst)
	t.Logf("child instance: %s", childInst)
	registerInstanceStopCleanup(t, bin, childInst)

	// Child should have the same HTTP server (entire VM state preserved).
	require.Eventually(t, func() bool {
		out, err := runCLI(bin, "--instance", childInst, "curl", "-sf", "http://127.0.0.1:8888/")
		return err == nil && strings.TrimSpace(out) == "original"
	}, 30*time.Second, time.Second, "child server never became ready")

	// Parent still works.
	parentOut := runCLISuccess(t, bin, "--instance", inst,
		"curl", "-sf", "http://127.0.0.1:8888/")
	assert.Equal(t, "original\n", parentOut)
}

func TestQEMU_ForkPipe(t *testing.T) {
	if parseQemuBackendForTest() == "" {
		t.Skip("requires LNX_BACKEND=qemu:...")
	}
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := "test-qemu-fork-pipe"
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	cmd, stderr, done := startTimedInstance(t, bin, inst, 60*time.Second)
	t.Cleanup(func() { cleanupStreamingCLI(t, cmd, done, stderr) })

	out := runCLISuccess(t, bin, "--instance", inst, "python3", "-u", "-c", `
import os
os.write(3, b"fork\n")
result = b""
while True:
    chunk = os.read(4, 4096)
    if not chunk:
        break
    result += chunk
    if b"\n" in result:
        break
text = result.decode().strip()
if text.startswith("error:"):
    print("FORK_ERROR: " + text)
elif text == "child":
    print("FORK_CHILD")
else:
    print("FORK_OK: " + text)
`)
	t.Logf("fork pipe output: %s", strings.TrimSpace(out))
	if strings.Contains(out, "FORK_OK:") {
		childInst := strings.TrimSpace(strings.TrimPrefix(
			strings.TrimSpace(out), "FORK_OK:"))
		registerInstanceStopCleanup(t, bin, childInst)
		assert.Contains(t, childInst, inst+"-fork-")
	} else {
		t.Fatalf("expected FORK_OK, got: %s", strings.TrimSpace(out))
	}
}

// --- helpers ---

// setupQemuTestDir creates a test dir on the same APFS volume as ~/.lnx
// so clonefile works for rootfs and ram.
func setupQemuTestDir(t *testing.T) string {
	t.Helper()
	home, _ := os.UserHomeDir()
	base := filepath.Join(home, ".lnx")

	kernelPath := filepath.Join(base, "vmlinuz")
	if _, err := os.Stat(kernelPath); err != nil {
		t.Skipf("skipping: vmlinuz not found")
	}
	rootfsPath := filepath.Join(base, "images", "default", "rootfs.ext4")
	if _, err := os.Stat(rootfsPath); err != nil {
		t.Skipf("skipping: rootfs not found")
	}

	initPath := filepath.Join("cmd", "lnx", "init")
	initBin, err := os.ReadFile(initPath)
	if err != nil {
		t.Skipf("skipping: guest init not found")
	}
	lnx.InitBinary = initBin

	// Create temp dir alongside rootfs (same APFS volume).
	dir, err := os.MkdirTemp(filepath.Dir(rootfsPath), "qemu-test-*")
	require.NoError(t, err)
	t.Cleanup(func() { os.RemoveAll(dir) })

	os.Symlink(kernelPath, filepath.Join(dir, "vmlinuz"))
	err = cloneFileForTest(rootfsPath, filepath.Join(dir, "rootfs.ext4"))
	require.NoError(t, err)
	return dir
}

func cloneFileForTest(src, dst string) error {
	return lnx.CloneFile(src, dst)
}

func parseQemuBackendForTest() string {
	// Mirror the logic in parseQemuBackend (unexported).
	val := lnx.ParseQemuBackend()
	return val
}

func apiClient(dir string) *http.Client {
	sockPath := dir + "/status.sock"
	return &http.Client{
		Transport: &http.Transport{
			DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
				return net.DialTimeout("unix", sockPath, 2*time.Second)
			},
		},
		Timeout: 10 * time.Second,
	}
}

func waitForExec(t *testing.T, client *http.Client, timeout time.Duration) {
	t.Helper()
	require.Eventually(t, func() bool {
		body, _ := json.Marshal(lnx.ExecRequest{Args: []string{"true"}})
		resp, err := client.Post("http://localhost/exec", "application/json", bytes.NewReader(body))
		if err != nil {
			return false
		}
		resp.Body.Close()
		return resp.StatusCode == http.StatusOK
	}, timeout, 500*time.Millisecond, "exec never became ready")
}

func execCommand(t *testing.T, client *http.Client, args ...string) string {
	t.Helper()
	body, err := json.Marshal(lnx.ExecRequest{Args: args})
	require.NoError(t, err)

	resp, err := client.Post("http://localhost/exec", "application/json", bytes.NewReader(body))
	require.NoError(t, err)
	defer resp.Body.Close()

	var output strings.Builder
	dec := json.NewDecoder(resp.Body)
	for {
		var msg map[string]json.RawMessage
		if err := dec.Decode(&msg); err != nil {
			break
		}
		if raw, ok := msg["stdout"]; ok {
			var s string
			json.Unmarshal(raw, &s)
			output.WriteString(s)
		}
	}
	return strings.TrimSpace(output.String())
}

func readCounter(t *testing.T, client *http.Client) int {
	t.Helper()
	out := execCommand(t, client, "cat", "/tmp/count.txt")
	val, err := strconv.Atoi(strings.TrimSpace(out))
	require.NoError(t, err, "parse counter value from %q", out)
	return val
}

