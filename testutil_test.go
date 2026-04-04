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
	srcDir := filepath.Join(home, ".lnx")

	for _, name := range []string{"vmlinuz", "rootfs.ext4"} {
		if _, err := os.Stat(filepath.Join(srcDir, name)); err != nil {
			t.Skipf("skipping: %s not found in ~/.lnx (run 'lnx init' first)", name)
		}
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

	os.Symlink(filepath.Join(srcDir, "vmlinuz"), filepath.Join(dir, "vmlinuz"))

	err = unix.Clonefile(
		filepath.Join(srcDir, "rootfs.ext4"),
		filepath.Join(dir, "rootfs.ext4"),
		0,
	)
	require.NoError(t, err)

	return dir
}

func testConfig(dir string) *lnx.Config {
	return &lnx.Config{
		KernelPath: filepath.Join(dir, "vmlinuz"),
		RootfsPath: filepath.Join(dir, "rootfs.ext4"),
	}
}
