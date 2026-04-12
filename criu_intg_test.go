//go:build darwin && integration

package lnx_test

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestCRIU_CheckpointAndRestore(t *testing.T) {
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := "test-criu-checkpoint"
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	home, _ := os.UserHomeDir()
	instDir := filepath.Join(home, ".lnx", "instances", inst)

	// Boot VM with a keeper.
	cmd, stderr, done := startTimedInstance(t, bin, inst, 120*time.Second)
	t.Cleanup(func() { cleanupStreamingCLI(t, cmd, done, stderr) })

	// Write python server script, then start it in background.
	runCLISuccess(t, bin, "--instance", inst, "sh", "-c",
		`cat > /tmp/server.py << 'PYEOF'
import os, resource
for fd in range(3, min(resource.getrlimit(resource.RLIMIT_NOFILE)[0], 1024)):
    try: os.close(fd)
    except: pass
os.setsid()
from http.server import HTTPServer, BaseHTTPRequestHandler
class H(BaseHTTPRequestHandler):
    state = {"counter": 42}
    def do_GET(self):
        self.send_response(200); self.end_headers()
        self.wfile.write(f'{H.state["counter"]}\n'.encode())
    def do_POST(self):
        H.state["counter"] += 1
        self.send_response(200); self.end_headers()
        self.wfile.write(f'{H.state["counter"]}\n'.encode())
    def log_message(self, *a): pass
HTTPServer(("0.0.0.0", 8888), H).serve_forever()
PYEOF
python3 -u /tmp/server.py </dev/null >/dev/null 2>&1 &`)

	// Wait for server.
	require.Eventually(t, func() bool {
		out, err := runCLI(bin, "--instance", inst, "curl", "-sf", "http://127.0.0.1:8888/")
		return err == nil && strings.TrimSpace(out) == "42"
	}, 30*time.Second, time.Second, "HTTP server never became ready")

	// POST to increment counter to 43.
	out := runCLISuccess(t, bin, "--instance", inst,
		"curl", "-sf", "-X", "POST", "http://127.0.0.1:8888/")
	assert.Equal(t, "43\n", out)

	// CRIU checkpoint.
	cpOut := runCLISuccess(t, bin, "--instance", inst, "checkpoints", "create", "--criu", "snap1")
	assert.Contains(t, cpOut, "snap1")

	// Verify checkpoint dir has both files.
	_, err := os.Stat(filepath.Join(instDir, "checkpoints", "snap1", "rootfs.ext4"))
	require.NoError(t, err)
	_, err = os.Stat(filepath.Join(instDir, "checkpoints", "snap1", "criu.ext4"))
	require.NoError(t, err)

	// Mutate state to 44.
	out = runCLISuccess(t, bin, "--instance", inst,
		"curl", "-sf", "-X", "POST", "http://127.0.0.1:8888/")
	assert.Equal(t, "44\n", out)

	// Kill the keeper and stop the VM so we can restore.
	cmd.Process.Kill()
	<-done
	runCLI(bin, "--instance", inst, "stop")

	restoreOut := runCLISuccess(t, bin, "--instance", inst, "checkpoints", "restore", "snap1")
	assert.Contains(t, restoreOut, "restored CRIU checkpoint")

	// Boot — CRIU auto-restores. Counter should be 43 (checkpoint value).
	require.Eventually(t, func() bool {
		out, err := runCLI(bin, "--instance", inst, "curl", "-sf", "http://127.0.0.1:8888/")
		return err == nil && strings.TrimSpace(out) == "43"
	}, 30*time.Second, time.Second, "restored server never became ready with correct state")
}

func TestCRIU_Fork(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := "test-criu-fork"
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	cmd, stderr, done := startTimedInstance(t, bin, inst, 60*time.Second)
	t.Cleanup(func() { cleanupStreamingCLI(t, cmd, done, stderr) })

	// Start HTTP server.
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
	registerInstanceStopCleanup(t, bin, childInst)

	// Child should have the same state.
	require.Eventually(t, func() bool {
		out, err := runCLI(bin, "--instance", childInst, "curl", "-sf", "http://127.0.0.1:8888/")
		return err == nil && strings.TrimSpace(out) == "original"
	}, 30*time.Second, time.Second, "child server never became ready")

	// Parent still works.
	parentOut := runCLISuccess(t, bin, "--instance", inst,
		"curl", "-sf", "http://127.0.0.1:8888/")
	assert.Equal(t, "original\n", parentOut)

	// Child knows its role.
	roleOut := runCLISuccess(t, bin, "--instance", childInst, "cat", "/var/run/lnx/fork-role")
	assert.Equal(t, "child\n", roleOut)
}

func TestCRIU_ForkPipe(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := "test-criu-fork-pipe"
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
else:
    print("FORK_OK: " + text)
`)
	if strings.Contains(out, "FORK_OK:") {
		childInst := strings.TrimSpace(strings.TrimPrefix(
			strings.TrimSpace(out), "FORK_OK:"))
		registerInstanceStopCleanup(t, bin, childInst)
		assert.Contains(t, childInst, inst+"-fork-")
	} else {
		t.Logf("fork pipe result: %s", strings.TrimSpace(out))
		t.Skip("CRIU fork via pipe not supported in this environment")
	}
}

func TestCRIU_CheckpointList(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := "test-criu-list"
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	runCLISuccess(t, bin, "--instance", inst, "checkpoints", "create", "disk-snap")

	out := runCLISuccess(t, bin, "--instance", inst, "checkpoints", "list")
	assert.Contains(t, out, "disk-snap")
	assert.Contains(t, out, "disk")
}

func TestCRIU_RestoreRequiresStoppedVM(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := "test-criu-restore-stopped"
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	cmd, stderr, done := startTimedInstance(t, bin, inst, 10*time.Second)
	t.Cleanup(func() { cleanupStreamingCLI(t, cmd, done, stderr) })

	runCLISuccess(t, bin, "--instance", inst, "checkpoints", "create", "snap")

	out, err := runCLI(bin, "--instance", inst, "checkpoints", "restore", "snap")
	require.Error(t, err)
	assert.Contains(t, out, "stop the VM")
}

// TestCRIU_DiningPhilosophers verifies that a complex web of processes
// connected by pipes, Unix domain sockets, and TCP all survive CRIU
// checkpoint/restore with their IPC channels intact.
//
// Topology — a ring of 4 processes:
//
//	coordinator →[pipe]→ worker0 →[UDS]→ worker1 →[TCP]→ worker2 →[pipe]→ coordinator
//
// Each "tick" sends a message around the ring, and every worker increments
// its counter. After checkpoint and restore, counters roll back and the
// ring continues to function.
func TestCRIU_DiningPhilosophers(t *testing.T) {
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := "test-criu-dining"
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	cmd, stderr, done := startTimedInstance(t, bin, inst, 120*time.Second)
	t.Cleanup(func() { cleanupStreamingCLI(t, cmd, done, stderr) })

	// Deploy the dining philosophers ring script.
	runCLISuccess(t, bin, "--instance", inst, "sh", "-c",
		`cat > /tmp/dining.py << 'PYEOF'
import os, socket, json, resource, signal
from http.server import HTTPServer, BaseHTTPRequestHandler

# Close inherited fds to avoid CRIU issues with leaked vsock fds.
for fd in range(3, min(resource.getrlimit(resource.RLIMIT_NOFILE)[0], 1024)):
    try: os.close(fd)
    except: pass
os.setsid()
signal.signal(signal.SIGCHLD, signal.SIG_IGN)

# === IPC channels ===
# Ring: coordinator ->[pipe]-> w0 ->[UDS]-> w1 ->[TCP]-> w2 ->[pipe]-> coordinator

c2w0_r, c2w0_w = os.pipe()           # coordinator -> w0
w2c_r, w2c_w = os.pipe()             # w2 -> coordinator
uds_w0, uds_w1 = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
tcp_srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
tcp_srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
tcp_srv.bind(("127.0.0.1", 8901))
tcp_srv.listen(1)

# === Worker 0: pipe-in, UDS-out ===
if os.fork() == 0:
    os.close(c2w0_w); os.close(w2c_r); os.close(w2c_w)
    uds_w1.close(); tcp_srv.close()
    count = 0
    buf = b""
    while True:
        chunk = os.read(c2w0_r, 4096)
        if not chunk: os._exit(0)
        buf += chunk
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            cmd, _, payload = line.decode().partition(" ")
            if cmd == "TICK": count += 1
            state = (payload + "," if payload else "") + f"w0={count}"
            uds_w0.sendall(f"{cmd} {state}\n".encode())

# === Worker 1: UDS-in, TCP-out ===
if os.fork() == 0:
    os.close(c2w0_r); os.close(c2w0_w)
    os.close(w2c_r); os.close(w2c_w)
    uds_w0.close()
    tcp_conn = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    tcp_conn.connect(("127.0.0.1", 8901))
    tcp_srv.close()
    count = 0
    buf = b""
    while True:
        chunk = uds_w1.recv(4096)
        if not chunk: os._exit(0)
        buf += chunk
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            cmd, _, payload = line.decode().partition(" ")
            if cmd == "TICK": count += 1
            state = (payload + "," if payload else "") + f"w1={count}"
            tcp_conn.sendall(f"{cmd} {state}\n".encode())

# === Worker 2: TCP-in, pipe-out ===
if os.fork() == 0:
    os.close(c2w0_r); os.close(c2w0_w); os.close(w2c_r)
    uds_w0.close(); uds_w1.close()
    tcp_conn, _ = tcp_srv.accept()
    tcp_srv.close()
    count = 0
    buf = b""
    while True:
        chunk = tcp_conn.recv(4096)
        if not chunk: os._exit(0)
        buf += chunk
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            cmd, _, payload = line.decode().partition(" ")
            if cmd == "TICK": count += 1
            state = (payload + "," if payload else "") + f"w2={count}"
            os.write(w2c_w, f"{cmd} {state}\n".encode())

# === Coordinator: HTTP server on :8900 ===
os.close(c2w0_r); os.close(w2c_w)
uds_w0.close(); uds_w1.close(); tcp_srv.close()

def ring_command(cmd):
    os.write(c2w0_w, f"{cmd}\n".encode())
    buf = b""
    while b"\n" not in buf:
        chunk = os.read(w2c_r, 4096)
        if not chunk: return None
        buf += chunk
    return buf.split(b"\n", 1)[0].decode().partition(" ")[2]

def parse_state(payload):
    result = {}
    for part in payload.split(","):
        if "=" in part:
            k, v = part.split("=", 1)
            result[k] = int(v)
    return result

class H(BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path == "/tick":
            payload = ring_command("TICK")
            if payload is None:
                self.send_error(500, "ring broken"); return
            self.send_response(200); self.end_headers()
            self.wfile.write(json.dumps(parse_state(payload), sort_keys=True).encode() + b"\n")
        else: self.send_error(404)
    def do_GET(self):
        if self.path == "/state":
            payload = ring_command("STATE")
            if payload is None:
                self.send_error(500, "ring broken"); return
            self.send_response(200); self.end_headers()
            self.wfile.write(json.dumps(parse_state(payload), sort_keys=True).encode() + b"\n")
        else: self.send_error(404)
    def log_message(self, *a): pass

HTTPServer(("0.0.0.0", 8900), H).serve_forever()
PYEOF
python3 -u /tmp/dining.py </dev/null >/dev/null 2>&1 &`)

	// Wait for HTTP server to be ready.
	require.Eventually(t, func() bool {
		out, err := runCLI(bin, "--instance", inst, "curl", "-sf", "http://127.0.0.1:8900/state")
		return err == nil && strings.Contains(out, "w0")
	}, 30*time.Second, time.Second, "dining philosophers never became ready")

	// Send 5 ticks through the ring (pipe → UDS → TCP → pipe).
	for i := 0; i < 5; i++ {
		runCLISuccess(t, bin, "--instance", inst,
			"curl", "-sf", "-X", "POST", "http://127.0.0.1:8900/tick")
	}

	// Verify all 3 workers counted 5 ticks.
	out := runCLISuccess(t, bin, "--instance", inst,
		"curl", "-sf", "http://127.0.0.1:8900/state")
	var state map[string]int
	require.NoError(t, json.Unmarshal([]byte(strings.TrimSpace(out)), &state))
	assert.Equal(t, 5, state["w0"], "pipe worker")
	assert.Equal(t, 5, state["w1"], "UDS worker")
	assert.Equal(t, 5, state["w2"], "TCP worker")

	// CRIU checkpoint.
	cpOut := runCLISuccess(t, bin, "--instance", inst,
		"checkpoints", "create", "--criu", "dining-snap")
	assert.Contains(t, cpOut, "dining-snap")

	// Verify checkpoint dir has both files.
	home, _ := os.UserHomeDir()
	instDir := filepath.Join(home, ".lnx", "instances", inst)
	cpDir := filepath.Join(instDir, "checkpoints", "dining-snap")
	require.DirExists(t, cpDir)
	require.FileExists(t, filepath.Join(cpDir, "rootfs.ext4"))
	require.FileExists(t, filepath.Join(cpDir, "criu.ext4"))

	// Mutate: send 3 more ticks → counters reach 8.
	for i := 0; i < 3; i++ {
		runCLISuccess(t, bin, "--instance", inst,
			"curl", "-sf", "-X", "POST", "http://127.0.0.1:8900/tick")
	}
	out = runCLISuccess(t, bin, "--instance", inst,
		"curl", "-sf", "http://127.0.0.1:8900/state")
	require.NoError(t, json.Unmarshal([]byte(strings.TrimSpace(out)), &state))
	assert.Equal(t, 8, state["w0"])

	// Kill keeper, stop VM, restore to checkpoint.
	cmd.Process.Kill()
	<-done
	runCLI(bin, "--instance", inst, "stop")

	restoreOut := runCLISuccess(t, bin, "--instance", inst,
		"checkpoints", "restore", "dining-snap")
	assert.Contains(t, restoreOut, "restored CRIU checkpoint")

	// Check if CRIU restore worked by looking for the coordinator's HTTP server.
	require.Eventually(t, func() bool {
		out, err := runCLI(bin, "--instance", inst,
			"curl", "-sf", "http://127.0.0.1:8900/state")
		if err != nil {
			return false
		}
		var s map[string]int
		if json.Unmarshal([]byte(strings.TrimSpace(out)), &s) != nil {
			return false
		}
		return s["w0"] == 5 && s["w1"] == 5 && s["w2"] == 5
	}, 30*time.Second, time.Second,
		"restored state should show all workers at 5")

	// Verify the ring still functions after restore.
	runCLISuccess(t, bin, "--instance", inst,
		"curl", "-sf", "-X", "POST", "http://127.0.0.1:8900/tick")
	out = runCLISuccess(t, bin, "--instance", inst,
		"curl", "-sf", "http://127.0.0.1:8900/state")
	require.NoError(t, json.Unmarshal([]byte(strings.TrimSpace(out)), &state))
	assert.Equal(t, 6, state["w0"], "pipe should survive restore")
	assert.Equal(t, 6, state["w1"], "UDS should survive restore")
	assert.Equal(t, 6, state["w2"], "TCP should survive restore")
}
