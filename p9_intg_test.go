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

func TestRun_9P_HomeReadable(t *testing.T) {
	t.Parallel()
	dir := setupTestDir(t)
	cfg := testConfig(dir)

	// Write a file in the host home dir. The guest should be able to
	// read it via the 9P mount.
	home, err := os.UserHomeDir()
	require.NoError(t, err)

	marker := filepath.Join(home, ".lnx_9p_test_marker")
	require.NoError(t, os.WriteFile(marker, []byte("9P_WORKS"), 0644))
	t.Cleanup(func() { os.Remove(marker) })

	exitCode, err := lnx.Run(cfg, "cat", marker)
	require.NoError(t, err)
	assert.Equal(t, 0, exitCode)
}

func TestRun_9P_HomeReadOnly(t *testing.T) {
	t.Parallel()
	dir := setupTestDir(t)
	cfg := testConfig(dir)

	home, err := os.UserHomeDir()
	require.NoError(t, err)

	// Attempting to write to the home 9P mount should fail (read-only).
	target := filepath.Join(home, ".lnx_9p_test_readonly")
	exitCode, err := lnx.Run(cfg, "sh", "-c", "echo fail > "+target)
	require.NoError(t, err)
	assert.NotEqual(t, 0, exitCode)

	// File should not exist on the host.
	_, err = os.Stat(target)
	assert.True(t, os.IsNotExist(err))
}
