//go:build darwin && integration

package lnx_test

import (
	"os"
	"path/filepath"
	"sync"
	"testing"

	"github.com/semistrict/lnx"
	"github.com/stretchr/testify/require"
	"golang.org/x/sys/unix"
)

var testDirOnce sync.Once

func setupTestDir(t *testing.T) string {
	t.Helper()

	home, _ := os.UserHomeDir()
	base := filepath.Join(home, ".lnx")

	kernelPath := filepath.Join(base, "vmlinuz")
	if _, err := os.Stat(kernelPath); err != nil {
		t.Skipf("skipping: vmlinuz not found in ~/.lnx (run 'lnx init' first)")
	}

	rootfsPath := filepath.Join(base, "instances", "default", "rootfs.ext4")
	if _, err := os.Stat(rootfsPath); err != nil {
		t.Skipf("skipping: rootfs.ext4 not found in ~/.lnx/instances/default/ (run 'lnx init' first)")
	}

	initPath := filepath.Join("cmd", "lnx", "init")
	if _, err := os.Stat(initPath); err != nil {
		t.Skipf("skipping: guest init binary not found at %s (run 'make' first)", initPath)
	}
	initBin, err := os.ReadFile(initPath)
	require.NoError(t, err)
	lnx.InitBinary = initBin

	testDirOnce.Do(func() { os.MkdirAll("tmp", 0755) })

	dir, err := os.MkdirTemp("tmp", "test-*")
	require.NoError(t, err)
	t.Cleanup(func() { os.RemoveAll(dir) })

	os.Symlink(kernelPath, filepath.Join(dir, "vmlinuz"))

	err = unix.Clonefile(rootfsPath, filepath.Join(dir, "rootfs.ext4"), 0)
	require.NoError(t, err)

	return dir
}

func testConfig(dir string) *lnx.Config {
	return &lnx.Config{
		KernelPath: filepath.Join(dir, "vmlinuz"),
		RootfsPath: filepath.Join(dir, "rootfs.ext4"),
	}
}
