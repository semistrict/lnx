//go:build darwin && integration

package lnx_test

import (
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"

	"github.com/creack/pty"
	"github.com/stretchr/testify/require"
	"github.com/vito/midterm"
	"golang.org/x/sys/unix"
)

// TestPTY_ShareDir verifies that `lnx share add` persists a directory
// and the next boot mounts it read-write in the guest.
func TestPTY_ShareDir(t *testing.T) {
	t.Parallel()

	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	// Create a dedicated instance with its own rootfs.
	home, _ := os.UserHomeDir()
	base := filepath.Join(home, ".lnx")
	instName := "test-share"
	imgDir := filepath.Join(base, "images", instName)
	instDir := filepath.Join(base, "instances", instName)
	defaultRootfs := findDefaultRootfs(base)
	if defaultRootfs == "" {
		t.Skipf("skipping: default instance rootfs not found")
	}

	os.MkdirAll(imgDir, 0755)
	os.MkdirAll(instDir, 0755)
	rootfs := filepath.Join(imgDir, "rootfs.ext4")
	os.Remove(rootfs)
	require.NoError(t, unix.Clonefile(defaultRootfs, rootfs, 0))
	t.Cleanup(func() {
		os.RemoveAll(imgDir)
		os.RemoveAll(instDir)
	})

	// Create a temp directory to share.
	shareDir := t.TempDir()
	require.NoError(t, os.WriteFile(filepath.Join(shareDir, "hello.txt"), []byte("SHARED_OK"), 0644))

	// Add the share via the CLI.
	addCmd := exec.Command(bin, "--instance", instName, "share", "add", shareDir)
	out, err := addCmd.CombinedOutput()
	require.NoError(t, err, "share add failed: %s", out)

	// Boot the instance and verify the share is mounted.
	term := midterm.NewTerminal(24, 80)
	cmd := exec.Command(bin, "--instance", instName, "sh", "-c",
		"cat "+shareDir+"/hello.txt; echo WRITE_TEST > "+shareDir+"/from_guest.txt")
	ptmx, err := pty.StartWithSize(cmd, &pty.Winsize{Rows: 24, Cols: 80})
	require.NoError(t, err)
	defer ptmx.Close()
	defer cmd.Process.Kill()

	go feedTerminal(term, ptmx)

	waitFor(t, term, "SHARED_OK", 15*time.Second)

	done := make(chan error, 1)
	go func() { done <- cmd.Wait() }()
	select {
	case <-done:
	case <-time.After(10 * time.Second):
		t.Fatal("VM did not exit within 10s")
	}

	// Verify the guest wrote to the shared dir (visible on host).
	data, err := os.ReadFile(filepath.Join(shareDir, "from_guest.txt"))
	require.NoError(t, err)
	require.Contains(t, string(data), "WRITE_TEST")
}
