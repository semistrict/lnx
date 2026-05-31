//go:build darwin && integration

package lnx_test

import (
	"os"
	"os/user"
	"path/filepath"
	"testing"

	"github.com/semistrict/lnx"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestRun_RunsAsHostUser(t *testing.T) {
	t.Parallel()
	dir := setupTestDir(t)
	cfg := testConfig(dir)

	u, err := user.Current()
	require.NoError(t, err)

	// Write output to a file in CWD (virtiofs), then check it on host.
	cwd := t.TempDir()
	cfg.CWD = cwd
	outFile := filepath.Join(cwd, "whoami.txt")

	exitCode, err := lnx.Run(cfg, "sh", "-c", "whoami > "+outFile)
	require.NoError(t, err)
	assert.Equal(t, 0, exitCode)

	data, err := os.ReadFile(outFile)
	require.NoError(t, err)
	assert.Contains(t, string(data), u.Username)
}

func TestRun_CWDIsHostCWD(t *testing.T) {
	t.Parallel()
	dir := setupTestDir(t)
	cfg := testConfig(dir)

	cwd := t.TempDir()
	cfg.CWD = cwd
	outFile := filepath.Join(cwd, "pwd.txt")

	exitCode, err := lnx.Run(cfg, "sh", "-c", "pwd > "+outFile)
	require.NoError(t, err)
	assert.Equal(t, 0, exitCode)

	data, err := os.ReadFile(outFile)
	require.NoError(t, err)
	assert.Contains(t, string(data), cwd)
}

func TestRun_ProfileDExists(t *testing.T) {
	t.Parallel()
	dir := setupTestDir(t)
	cfg := testConfig(dir)

	exitCode, err := lnx.Run(cfg, "test", "-f", "/etc/profile.d/lnx-bashrc.sh")
	require.NoError(t, err)
	assert.Equal(t, 0, exitCode)
}

func TestRun_NotRoot(t *testing.T) {
	t.Parallel()
	dir := setupTestDir(t)
	cfg := testConfig(dir)

	cwd := t.TempDir()
	cfg.CWD = cwd
	outFile := filepath.Join(cwd, "id.txt")

	exitCode, err := lnx.Run(cfg, "sh", "-c", "id -u > "+outFile)
	require.NoError(t, err)
	assert.Equal(t, 0, exitCode)

	data, err := os.ReadFile(outFile)
	require.NoError(t, err)
	// Should NOT be root (uid 0)
	assert.NotContains(t, string(data), "0\n")
}
