//go:build darwin && integration

package lnx_test

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/semistrict/lnx"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestRun_VirtioFS_CWDMountedAtSamePath(t *testing.T) {
	dir := setupTestDir(t)
	cfg := testConfig(dir)

	// Use a known host directory as CWD.
	cwd := t.TempDir()
	cfg.CWD = cwd

	// Write a file on the host.
	require.NoError(t, os.WriteFile(filepath.Join(cwd, "host.txt"), []byte("from-host"), 0644))

	// Read it from inside the VM at the same absolute path.
	exitCode, err := lnx.Run(cfg, "cat", filepath.Join(cwd, "host.txt"))
	require.NoError(t, err)
	assert.Equal(t, 0, exitCode)
}

func TestRun_VirtioFS_GuestWriteVisibleOnHost(t *testing.T) {
	dir := setupTestDir(t)
	cfg := testConfig(dir)

	cwd := t.TempDir()
	cfg.CWD = cwd

	guestFile := filepath.Join(cwd, "guest.txt")

	exitCode, err := lnx.Run(cfg, "sh", "-c", "echo from-guest > "+guestFile)
	require.NoError(t, err)
	assert.Equal(t, 0, exitCode)

	// Verify the file appeared on the host.
	data, err := os.ReadFile(guestFile)
	require.NoError(t, err)
	assert.Equal(t, "from-guest\n", string(data))
}
