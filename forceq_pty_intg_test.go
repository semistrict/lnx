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

// TestPTY_DoubleCtrlC_ForceQuit verifies that double Ctrl-C in raw mode
// (where ISIG is disabled) force-quits the VM with exit code 130.
func TestPTY_DoubleCtrlC_ForceQuit(t *testing.T) {
	t.Parallel()

	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	term := midterm.NewTerminal(24, 80)

	// Use --ephemeral so we don't contend on the rootfs lock.
	// Use a script that traps SIGINT so the first Ctrl-C doesn't kill it.
	cmd := exec.Command(bin, "--ephemeral", "sh", "-c", "trap '' INT; echo READY; sleep 3600")
	ptmx, err := pty.StartWithSize(cmd, &pty.Winsize{Rows: 24, Cols: 80})
	if err != nil {
		t.Fatalf("start pty: %v", err)
	}
	defer ptmx.Close()
	defer cmd.Process.Kill()

	go feedTerminal(term, ptmx)

	// Wait for guest to print READY.
	waitFor(t, term, "READY", 15*time.Second)

	// Send double Ctrl-C (0x03) quickly.
	ptmx.Write([]byte{0x03})
	time.Sleep(200 * time.Millisecond)
	ptmx.Write([]byte{0x03})

	done := make(chan error, 1)
	go func() { done <- cmd.Wait() }()

	select {
	case err := <-done:
		if exitErr, ok := err.(*exec.ExitError); ok {
			if exitErr.ExitCode() == 130 {
				return // success
			}
			t.Fatalf("expected exit code 130, got %d", exitErr.ExitCode())
		}
		if err == nil {
			t.Fatal("expected exit code 130, got 0")
		}
		t.Fatalf("unexpected error: %v", err)
	case <-time.After(15 * time.Second):
		t.Fatal("double Ctrl-C did not force-quit within 15s")
	}
}

// TestPTY_DoubleCtrlC_CobraPath tests force-quit when lnx goes through cobra
// (e.g. `lnx --instance foo bash -l`). This is distinct from the bypass path
// tested above — cobra-parsed flags previously couldn't reach the bypass path.
func TestPTY_DoubleCtrlC_CobraPath(t *testing.T) {
	t.Parallel()

	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	// Create a dedicated instance.
	home, _ := os.UserHomeDir()
	base := filepath.Join(home, ".lnx")
	instDir := filepath.Join(base, "instances", "test-forceq-cobra")
	defaultRootfs := filepath.Join(base, "instances", "default", "rootfs.ext4")

	if _, err := os.Stat(defaultRootfs); err != nil {
		t.Skipf("skipping: default instance rootfs not found (run 'lnx init' first)")
	}

	os.MkdirAll(instDir, 0755)
	rootfs := filepath.Join(instDir, "rootfs.ext4")
	os.Remove(rootfs)
	require.NoError(t, unix.Clonefile(defaultRootfs, rootfs, 0))
	t.Cleanup(func() { os.RemoveAll(instDir) })

	term := midterm.NewTerminal(24, 80)

	// --instance forces cobra path. The guest command traps SIGINT.
	cmd := exec.Command(bin, "--instance", "test-forceq-cobra", "sh", "-c", "trap '' INT; echo READY; sleep 3600")
	ptmx, err := pty.StartWithSize(cmd, &pty.Winsize{Rows: 24, Cols: 80})
	if err != nil {
		t.Fatalf("start pty: %v", err)
	}
	defer ptmx.Close()
	defer cmd.Process.Kill()

	go feedTerminal(term, ptmx)

	waitFor(t, term, "READY", 15*time.Second)

	// Double Ctrl-C.
	ptmx.Write([]byte{0x03})
	time.Sleep(200 * time.Millisecond)
	ptmx.Write([]byte{0x03})

	done := make(chan error, 1)
	go func() { done <- cmd.Wait() }()

	select {
	case err := <-done:
		if exitErr, ok := err.(*exec.ExitError); ok {
			if exitErr.ExitCode() == 130 {
				return
			}
			t.Fatalf("expected exit code 130, got %d", exitErr.ExitCode())
		}
		if err == nil {
			t.Fatal("expected exit code 130, got 0")
		}
		t.Fatalf("unexpected error: %v", err)
	case <-time.After(15 * time.Second):
		t.Fatal("double Ctrl-C did not force-quit within 15s")
	}
}
