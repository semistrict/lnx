//go:build darwin && integration

package lnx_test

import (
	"os"
	"syscall"
	"testing"
	"time"

	"github.com/semistrict/lnx"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

// TestRun_ForceQuit_RootfsIntact verifies that after a VM is force-killed
// (double Ctrl-C), the rootfs is intact and can boot again.
// This test is NOT parallel because it sends SIGINT to the process.
func TestRun_ForceQuit_RootfsIntact(t *testing.T) {
	dir := setupTestDir(t)
	cfg := testConfig(dir)

	// Write a file to the rootfs, sync, then trap signals and block forever.
	script := `echo CANARY > $HOME/canary.txt && sync && trap "" INT TERM && sleep 3600`

	errCh := make(chan error, 1)
	codeCh := make(chan int, 1)
	go func() {
		code, err := lnx.Run(cfg, "sh", "-c", script)
		errCh <- err
		codeCh <- code
	}()

	// Wait for the VM to boot and write the file.
	time.Sleep(10 * time.Second)

	// Double SIGINT triggers force quit.
	pid := os.Getpid()
	syscall.Kill(pid, syscall.SIGINT)
	time.Sleep(100 * time.Millisecond)
	syscall.Kill(pid, syscall.SIGINT)

	err := <-errCh
	code := <-codeCh
	require.NoError(t, err)
	assert.Equal(t, 130, code)

	// Boot again on the same rootfs — verify ext4 survived force kill.
	exitCode, err := lnx.Run(cfg, "sh", "-c", "cat $HOME/canary.txt")
	require.NoError(t, err)
	assert.Equal(t, 0, exitCode)
}
