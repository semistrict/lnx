//go:build darwin && integration

package lnx_test

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

// TestMemoryCheckpointCreateAndRestore verifies the full memory checkpoint
// cycle: boot VM, start a background counter in process memory, create a
// memory checkpoint (hibernate + clone rootfs + swap), verify checkpoint
// directory, restore from checkpoint, and verify the counter is back to
// its checkpoint-time value.
func TestMemoryCheckpointCreateAndRestore(t *testing.T) {
	bin, err := filepath.Abs("lnx")
	require.NoError(t, err)
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("skipping: repo lnx binary not found at %s", bin)
	}

	home, _ := os.UserHomeDir()
	base := filepath.Join(home, ".lnx")

	inst := "test-mem-checkpoint"
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)
	instDir := filepath.Join(base, "instances", inst)

	// Boot and start a background counter that writes to /tmp (tmpfs).
	// The counter only exists in process memory — it won't survive a cold boot.
	runCLISuccess(t, bin, "--instance", inst,
		"sh", "-lc", `nohup sh -c 'i=1000; while true; do echo $i > /tmp/counter; i=$((i+1)); sleep 1; done' >/dev/null 2>&1 &
		sleep 3
		echo "pre-checkpoint: $(cat /tmp/counter)"`)

	// Create a memory checkpoint. The counter command above kept the daemon
	// alive via the idle timeout (5s). The checkpoint command connects to
	// the running daemon's API. With the RunDaemon reboot loop, the VM
	// hibernates, clones, and auto-resumes — the CLI blocks until done.
	cpOut := runCLISuccess(t, bin, "--instance", inst, "checkpoints", "create", "--memory",
		"--description", "After starting counter",
		"--tag", "stable",
		"test-cp")
	assert.Contains(t, cpOut, "test-cp")

	// Verify checkpoint directory structure.
	cpDir := filepath.Join(instDir, "checkpoints", "test-cp")
	_, err = os.Stat(filepath.Join(cpDir, "rootfs.ext4"))
	require.NoError(t, err, "checkpoint rootfs.ext4 should exist")
	_, err = os.Stat(filepath.Join(cpDir, "swap.img"))
	require.NoError(t, err, "checkpoint swap.img should exist")
	_, err = os.Stat(filepath.Join(cpDir, "metadata.json"))
	require.NoError(t, err, "checkpoint metadata.json should exist")

	// Verify metadata contents.
	metaBytes, err := os.ReadFile(filepath.Join(cpDir, "metadata.json"))
	require.NoError(t, err)
	var meta struct {
		Name        string   `json:"name"`
		Type        string   `json:"type"`
		Description string   `json:"description"`
		Tags        []string `json:"tags"`
		CreatedAt   string   `json:"created_at"`
	}
	require.NoError(t, json.Unmarshal(metaBytes, &meta))
	assert.Equal(t, "test-cp", meta.Name)
	assert.Equal(t, "memory", meta.Type)
	assert.Equal(t, "After starting counter", meta.Description)
	assert.Equal(t, []string{"stable"}, meta.Tags)
	assert.NotEmpty(t, meta.CreatedAt)

	// Restore from the memory checkpoint. The daemon shuts down, replaces
	// rootfs+swap, and auto-reboots (kernel resumes from checkpoint).
	// The command blocks until the VM is back up.
	restoreOut := runCLISuccess(t, bin, "--instance", inst, "checkpoints", "restore", "test-cp")
	assert.Contains(t, restoreOut, "restored")

	// The VM is already running (auto-resumed). The counter process should
	// still be running with its checkpoint-time value (~1002-1004).
	out := runCLISuccess(t, bin, "--instance", inst,
		"sh", "-lc", `sleep 2; val=$(cat /tmp/counter 2>/dev/null); echo "post-restore: $val"`)
	assert.Contains(t, out, "post-restore: 10")

	// Clean up.
	runCLISuccess(t, bin, "--instance", inst, "stop", "--shutdown")
}

// TestCheckpointListWithMetadata verifies that `lnx checkpoints list` shows
// both legacy disk-only checkpoints and memory checkpoints with type,
// description, and tags columns.
func TestCheckpointListWithMetadata(t *testing.T) {
	bin, err := filepath.Abs("lnx")
	require.NoError(t, err)
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("skipping: repo lnx binary not found at %s", bin)
	}

	home, _ := os.UserHomeDir()
	base := filepath.Join(home, ".lnx")

	inst := "test-cp-list-meta"
	createClonedInstance(t, inst)
	instDir := filepath.Join(base, "instances", inst)
	cpBaseDir := filepath.Join(instDir, "checkpoints")

	// Create a legacy disk-only checkpoint (no VM needed).
	runCLISuccess(t, bin, "--instance", inst, "checkpoints", "create", "disk-cp")

	// Manually create a memory checkpoint directory structure.
	memCPDir := filepath.Join(cpBaseDir, "mem-cp")
	require.NoError(t, os.MkdirAll(memCPDir, 0755))
	meta := map[string]any{
		"name":        "mem-cp",
		"type":        "memory",
		"description": "Test memory checkpoint",
		"tags":        []string{"v1", "stable"},
		"created_at":  time.Now().Format(time.RFC3339),
	}
	metaBytes, err := json.MarshalIndent(meta, "", "  ")
	require.NoError(t, err)
	require.NoError(t, os.WriteFile(filepath.Join(memCPDir, "metadata.json"), metaBytes, 0644))
	// Create dummy rootfs and swap so the structure is complete.
	require.NoError(t, os.WriteFile(filepath.Join(memCPDir, "rootfs.ext4"), []byte("dummy"), 0644))
	require.NoError(t, os.WriteFile(filepath.Join(memCPDir, "swap.img"), []byte("dummy"), 0644))

	// Verify both appear in the list.
	listOut := runCLISuccess(t, bin, "--instance", inst, "checkpoints", "list")
	assert.Contains(t, listOut, "disk-cp")
	assert.Contains(t, listOut, "mem-cp")
	assert.Contains(t, listOut, "disk")
	assert.Contains(t, listOut, "memory")
	assert.Contains(t, listOut, "Test memory checkpoint")
	assert.Contains(t, listOut, "v1")
}

// TestCheckpointDeleteMemory verifies that `lnx checkpoints delete` removes
// both legacy disk-only checkpoints and memory checkpoint directories.
func TestCheckpointDeleteMemory(t *testing.T) {
	bin, err := filepath.Abs("lnx")
	require.NoError(t, err)
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("skipping: repo lnx binary not found at %s", bin)
	}

	home, _ := os.UserHomeDir()
	base := filepath.Join(home, ".lnx")

	inst := "test-cp-delete"
	createClonedInstance(t, inst)
	instDir := filepath.Join(base, "instances", inst)
	cpBaseDir := filepath.Join(instDir, "checkpoints")

	// Create a disk checkpoint (no VM needed).
	runCLISuccess(t, bin, "--instance", inst, "checkpoints", "create", "to-delete-disk")

	// Manually create a memory checkpoint directory structure.
	memCPDir := filepath.Join(cpBaseDir, "to-delete-mem")
	require.NoError(t, os.MkdirAll(memCPDir, 0755))
	meta := map[string]any{
		"name":        "to-delete-mem",
		"type":        "memory",
		"description": "To be deleted",
		"tags":        []string{},
		"created_at":  time.Now().Format(time.RFC3339),
	}
	metaBytes, err := json.MarshalIndent(meta, "", "  ")
	require.NoError(t, err)
	require.NoError(t, os.WriteFile(filepath.Join(memCPDir, "metadata.json"), metaBytes, 0644))
	require.NoError(t, os.WriteFile(filepath.Join(memCPDir, "rootfs.ext4"), []byte("dummy"), 0644))
	require.NoError(t, os.WriteFile(filepath.Join(memCPDir, "swap.img"), []byte("dummy"), 0644))

	// Verify they exist.
	_, err = os.Stat(filepath.Join(cpBaseDir, "to-delete-disk.ext4"))
	require.NoError(t, err)
	_, err = os.Stat(filepath.Join(memCPDir, "metadata.json"))
	require.NoError(t, err)

	// Delete the disk checkpoint.
	delOut := runCLISuccess(t, bin, "--instance", inst, "checkpoints", "delete", "to-delete-disk")
	assert.Contains(t, delOut, "deleted")
	_, err = os.Stat(filepath.Join(cpBaseDir, "to-delete-disk.ext4"))
	assert.True(t, os.IsNotExist(err), "disk checkpoint should be gone")

	// Delete the memory checkpoint.
	delOut = runCLISuccess(t, bin, "--instance", inst, "checkpoints", "delete", "to-delete-mem")
	assert.Contains(t, delOut, "deleted")
	_, err = os.Stat(memCPDir)
	assert.True(t, os.IsNotExist(err), "memory checkpoint dir should be gone")
}

// TestMemoryCheckpointFromGuest verifies that a guest process can create a
// memory checkpoint by curling the guest control socket.
func TestMemoryCheckpointFromGuest(t *testing.T) {
	bin, err := filepath.Abs("lnx")
	require.NoError(t, err)
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("skipping: repo lnx binary not found at %s", bin)
	}

	home, _ := os.UserHomeDir()
	base := filepath.Join(home, ".lnx")

	inst := "test-mem-cp-guest"
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)
	instDir := filepath.Join(base, "instances", inst)

	// Boot VM, start a background counter, and create a memory checkpoint
	// from inside the guest via the control socket.
	runCLISuccess(t, bin, "--instance", inst,
		"sh", "-lc", `nohup sh -c 'i=1000; while true; do echo $i > /tmp/counter; i=$((i+1)); sleep 1; done' >/dev/null 2>&1 &
		sleep 2
		curl -sf -X POST --unix-socket /var/run/lnx/control.sock \
			-H 'Content-Type: application/json' \
			-d '{"name":"from-guest","description":"guest-initiated","tags":["auto"]}' \
			http://localhost/checkpoint/memory`)

	// The VM hibernated and auto-resumed (RunDaemon reboot loop).
	// Wait briefly for the checkpoint files to be written.
	time.Sleep(2 * time.Second)

	// Verify checkpoint directory exists on the host.
	cpDir := filepath.Join(instDir, "checkpoints", "from-guest")
	_, err = os.Stat(filepath.Join(cpDir, "rootfs.ext4"))
	require.NoError(t, err, "guest-created checkpoint rootfs should exist")
	_, err = os.Stat(filepath.Join(cpDir, "swap.img"))
	require.NoError(t, err, "guest-created checkpoint swap should exist")

	metaBytes, err := os.ReadFile(filepath.Join(cpDir, "metadata.json"))
	require.NoError(t, err)
	var meta struct {
		Name        string   `json:"name"`
		Type        string   `json:"type"`
		Description string   `json:"description"`
		Tags        []string `json:"tags"`
	}
	require.NoError(t, json.Unmarshal(metaBytes, &meta))
	assert.Equal(t, "from-guest", meta.Name)
	assert.Equal(t, "memory", meta.Type)
	assert.Equal(t, "guest-initiated", meta.Description)
	assert.Equal(t, []string{"auto"}, meta.Tags)
}
