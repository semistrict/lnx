//go:build darwin && integration

package lnx_test

import (
	"context"
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestCLI_CloneWithMemoryPreservesProcessAndPorts(t *testing.T) {
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	srcInst := "test-memorysnapshot-src"
	dstInst := "test-memorysnapshot-dst"
	createClonedInstance(t, srcInst)
	registerInstanceStopCleanup(t, bin, srcInst, dstInst)
	t.Cleanup(func() {
		home, _ := os.UserHomeDir()
		_ = os.RemoveAll(filepath.Join(home, ".lnx", "instances", dstInst))
	})

	env := append(os.Environ(), "LNX_EXPERIMENTS=memorysnapshot")
	requireMemorySnapshotPrereqs(t, bin, env)

	cmd, _, stderr, done := startStreamingCLIEnv(t, bin, env, "--instance", srcInst, "sh", "-c", "sleep 20")
	t.Cleanup(func() { cleanupStreamingCLI(t, cmd, done, stderr) })
	waitForActiveSession(t, srcInst, 30*time.Second)

	runCLISuccessEnv(t, bin, env, "--instance", srcInst, "sh", "-lc", `
sleep 120 >/tmp/lnx-memorysleep.log 2>&1 & echo $! > /tmp/lnx-memorysleep.pid
mkdir -p /tmp/lnx-http-root
printf 'memorysnapshot-ok\n' > /tmp/lnx-http-root/index.html
cd /tmp/lnx-http-root
python3 -m http.server 18080 >/tmp/lnx-http.log 2>&1 & echo $! > /tmp/lnx-http.pid
`)

	require.Eventually(t, func() bool {
		out, err := runCLIEnv(bin, env, "--instance", srcInst, "sh", "-lc", `curl -fsS http://127.0.0.1:18080/ | head -n 1`)
		return err == nil && strings.Contains(out, "memorysnapshot-ok")
	}, 20*time.Second, 500*time.Millisecond)

	cloneOut := runCLISuccessEnv(t, bin, env, "--instance", srcInst, "clone", dstInst)
	assert.Contains(t, cloneOut, `created instance "`+dstInst+`" from "`+srcInst+`"`)

	srcSnapshotsDir := filepath.Join(os.Getenv("HOME"), ".lnx", "instances", srcInst, "memorysnapshots")
	entries, err := os.ReadDir(srcSnapshotsDir)
	if err != nil && !os.IsNotExist(err) {
		t.Fatalf("read source memorysnapshots dir: %v", err)
	}
	assert.Len(t, entries, 0)

	procOut := runCLISuccessEnv(t, bin, env, "--instance", dstInst, "sh", "-lc", `ps -eo args | grep -F 'sleep 120' | grep -v grep && echo RUNNING`)
	assert.Contains(t, procOut, "RUNNING")

	httpOut := runCLISuccessEnv(t, bin, env, "--instance", dstInst, "sh", "-lc", `curl -fsS http://127.0.0.1:18080/ | head -n 1`)
	assert.Contains(t, httpOut, "memorysnapshot-ok")

	require.Eventually(t, func() bool {
		out, err := runCLIEnv(bin, env, "--instance", dstInst, "ports", "list")
		return err == nil && strings.Contains(out, "18080")
	}, 20*time.Second, 500*time.Millisecond)
}

func requireMemorySnapshotPrereqs(t *testing.T, bin string, env []string) {
	t.Helper()

	out, err := runCLIEnv(bin, env, "true")
	if err == nil {
		return
	}
	if strings.Contains(out, "vmlinuz-firecracker not found") ||
		strings.Contains(out, "memorysnapshot/boot-error.log") ||
		strings.Contains(out, "Kvm error:") ||
		strings.Contains(out, "/dev/kvm") {
		t.Skip("skipping: outer VM cannot launch nested Firecracker")
	}
	require.NoError(t, err, out)
}

func waitForActiveSession(t *testing.T, instance string, timeout time.Duration) {
	t.Helper()

	require.Eventually(t, func() bool {
		sessions, err := fetchSessions(instance)
		return err == nil && len(sessions) > 0
	}, timeout, 500*time.Millisecond)
}

func fetchSessions(instance string) ([]map[string]any, error) {
	sockPath := filepath.Join(os.Getenv("HOME"), ".lnx", "instances", instance, "status.sock")
	client := &http.Client{
		Transport: &http.Transport{
			DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
				var d net.Dialer
				return d.DialContext(ctx, "unix", sockPath)
			},
		},
	}
	resp, err := client.Get("http://localhost/sessions")
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode/100 != 2 {
		return nil, fmt.Errorf("sessions status %s", resp.Status)
	}
	var sessions []map[string]any
	if err := json.NewDecoder(resp.Body).Decode(&sessions); err != nil {
		return nil, err
	}
	return sessions, nil
}
