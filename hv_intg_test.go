//go:build darwin && integration

package lnx_test

import (
	"bytes"
	"context"
	"encoding/json"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/semistrict/lnx"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

// hvConfig returns a Config for the HV backend with a cloned rootfs.
func hvConfig(t *testing.T) (*lnx.Config, string) {
	t.Helper()
	dir := setupTestDir(t)
	cfg := &lnx.Config{
		KernelPath:  filepath.Join(dir, "vmlinuz"),
		RootfsPath:  filepath.Join(dir, "rootfs.ext4"),
		Backend:     "hv",
		CPUs:        1,
		MemoryBytes: 512 << 20,
		SocketDir:   dir,
	}
	return cfg, dir
}

// TestHV_RunEchoHello exercises the full path: bootVM → vsock control →
// guest init → exec server → run "echo hello" → exit code 0.
// This is the single most important test — if vsock interrupt delivery
// is broken, this fails.
func TestHV_RunEchoHello(t *testing.T) {
	t.Parallel()
	cfg, _ := hvConfig(t)
	cfg.UARTWriter = os.Stderr // show kernel + init output
	exitCode, err := lnx.Run(cfg, "echo", "hello")
	require.NoError(t, err)
	assert.Equal(t, 0, exitCode)
}

// TestHV_RunExitCodes verifies that the guest exit code propagates
// through the vsock exec protocol.
func TestHV_RunExitCodes(t *testing.T) {
	t.Parallel()
	cfg, _ := hvConfig(t)

	exitCode, err := lnx.Run(cfg, "true")
	require.NoError(t, err)
	assert.Equal(t, 0, exitCode)

	exitCode, err = lnx.Run(cfg, "false")
	require.NoError(t, err)
	assert.Equal(t, 1, exitCode)

	exitCode, err = lnx.Run(cfg, "sh", "-c", "exit 42")
	require.NoError(t, err)
	assert.Equal(t, 42, exitCode)
}

// TestHV_RunCommandNotFound verifies unknown commands return 127.
func TestHV_RunCommandNotFound(t *testing.T) {
	t.Parallel()
	cfg, _ := hvConfig(t)
	exitCode, err := lnx.Run(cfg, "nonexistent_command_xyz")
	require.NoError(t, err)
	assert.Equal(t, 127, exitCode)
}

// TestHV_RunShellPipeline tests a multi-process pipeline to exercise
// concurrent I/O through virtio-vsock.
func TestHV_RunShellPipeline(t *testing.T) {
	t.Parallel()
	cfg, _ := hvConfig(t)
	exitCode, err := lnx.Run(cfg, "sh", "-c", "echo ABCDEF | tr A-F a-f")
	require.NoError(t, err)
	assert.Equal(t, 0, exitCode)
}

// TestHV_RunLargeOutput tests that large stdout doesn't deadlock or corrupt
// the vsock data path. Exercises flow control / credit updates.
func TestHV_RunLargeOutput(t *testing.T) {
	t.Parallel()
	cfg, _ := hvConfig(t)
	// Generate ~100KB of output.
	exitCode, err := lnx.Run(cfg, "sh", "-c", "seq 1 10000")
	require.NoError(t, err)
	assert.Equal(t, 0, exitCode)
}

// TestHV_RunEnvironment verifies that environment variables are passed
// through the vsock exec protocol.
func TestHV_RunEnvironment(t *testing.T) {
	t.Parallel()
	cfg, _ := hvConfig(t)
	cfg.Env = []string{"LNX_TEST_VAR=hello_from_host"}
	exitCode, err := lnx.Run(cfg, "sh", "-c", `test "$LNX_TEST_VAR" = "hello_from_host"`)
	require.NoError(t, err)
	assert.Equal(t, 0, exitCode)
}

// TestHV_RunCWD verifies that the working directory is set correctly.
func TestHV_RunCWD(t *testing.T) {
	t.Parallel()
	cfg, _ := hvConfig(t)
	cfg.CWD = "/tmp"
	exitCode, err := lnx.Run(cfg, "sh", "-c", `test "$(pwd)" = "/tmp"`)
	require.NoError(t, err)
	assert.Equal(t, 0, exitCode)
}

// TestHV_ExecIntoRunningVM boots a long-running command, then execs a
// second command into the same VM via the HTTP API. This tests concurrent
// vsock sessions and the daemon API path.
func TestHV_ExecIntoRunningVM(t *testing.T) {
	t.Parallel()
	cfg, dir := hvConfig(t)

	// Boot VM with a long-running command.
	go lnx.Run(cfg, "sleep", "60")

	sockPath := filepath.Join(dir, "status.sock")
	client := &http.Client{
		Transport: &http.Transport{
			DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
				return net.DialTimeout("unix", sockPath, 2*time.Second)
			},
		},
	}

	// Wait until exec endpoint is ready.
	require.Eventually(t, func() bool {
		body, _ := json.Marshal(lnx.ExecRequest{Args: []string{"true"}})
		resp, err := client.Post("http://localhost/exec", "application/json", bytes.NewReader(body))
		if err != nil {
			return false
		}
		resp.Body.Close()
		return resp.StatusCode == http.StatusOK
	}, 30*time.Second, 500*time.Millisecond, "VM exec never became ready")

	// Exec into the running VM.
	body, err := json.Marshal(lnx.ExecRequest{Args: []string{"echo", "HV_EXEC_WORKS"}})
	require.NoError(t, err)

	resp, err := client.Post("http://localhost/exec", "application/json", bytes.NewReader(body))
	require.NoError(t, err)
	defer resp.Body.Close()
	assert.Equal(t, http.StatusOK, resp.StatusCode)

	var output string
	var exitCode int = -1
	dec := json.NewDecoder(resp.Body)
	for {
		var msg map[string]json.RawMessage
		if err := dec.Decode(&msg); err != nil {
			break
		}
		if raw, ok := msg["stdout"]; ok {
			var s string
			json.Unmarshal(raw, &s)
			output += s
		}
		if raw, ok := msg["exit_code"]; ok {
			json.Unmarshal(raw, &exitCode)
		}
	}

	assert.Contains(t, output, "HV_EXEC_WORKS")
	assert.Equal(t, 0, exitCode)
}
