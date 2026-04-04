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

func TestRun_EchoHello(t *testing.T) {
	t.Parallel()
	dir := setupTestDir(t)
	exitCode, err := lnx.Run(testConfig(dir), "echo", "hello")
	require.NoError(t, err)
	assert.Equal(t, 0, exitCode)
}

func TestRun_ExitCodeZero(t *testing.T) {
	t.Parallel()
	dir := setupTestDir(t)
	exitCode, err := lnx.Run(testConfig(dir), "true")
	require.NoError(t, err)
	assert.Equal(t, 0, exitCode)
}

func TestRun_ExitCodeOne(t *testing.T) {
	t.Parallel()
	dir := setupTestDir(t)
	exitCode, err := lnx.Run(testConfig(dir), "false")
	require.NoError(t, err)
	assert.Equal(t, 1, exitCode)
}

func TestRun_ExitCodeCustom(t *testing.T) {
	t.Parallel()
	dir := setupTestDir(t)
	exitCode, err := lnx.Run(testConfig(dir), "sh", "-c", "exit 42")
	require.NoError(t, err)
	assert.Equal(t, 42, exitCode)
}

func TestRun_OnlineResize(t *testing.T) {
	t.Parallel()
	dir := setupTestDir(t)
	cfg := testConfig(dir)

	f, err := os.OpenFile(cfg.RootfsPath, os.O_RDWR, 0)
	require.NoError(t, err)
	require.NoError(t, f.Truncate(8*1024*1024*1024))
	require.NoError(t, f.Close())

	exitCode, err := lnx.Run(cfg, "sh", "-c", "df -BG / | tail -1 | awk '{print $2}'")
	require.NoError(t, err)
	assert.Equal(t, 0, exitCode)

	logBytes, err := os.ReadFile(filepath.Join(dir, "lnx.log"))
	require.NoError(t, err)
	logStr := string(logBytes)
	assert.Contains(t, logStr, "resize2fs")
	assert.NotContains(t, logStr, "Nothing to do")
}

func TestRun_MissingKernel(t *testing.T) {
	t.Parallel()
	lnx.InitBinary = []byte("fake")
	_, err := lnx.Run(&lnx.Config{
		KernelPath: "/nonexistent/vmlinuz",
		RootfsPath: "/nonexistent/rootfs.ext4",
	}, "echo", "hello")
	require.Error(t, err)
	require.Contains(t, err.Error(), "not found")
}
