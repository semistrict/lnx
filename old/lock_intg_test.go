//go:build darwin && integration

package lnx_test

import (
	"os"
	"sync"
	"testing"
	"time"

	"github.com/semistrict/lnx"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestRun_ConcurrentLock(t *testing.T) {
	t.Parallel()
	dir := setupTestDir(t)
	cfg := testConfig(dir)

	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		lnx.Run(cfg, "sleep", "1")
	}()

	pidPath := cfg.RootfsPath + ".pid"
	require.Eventually(t, func() bool {
		_, err := os.Stat(pidPath)
		return err == nil
	}, 5*time.Second, 10*time.Millisecond, "pidfile never appeared")

	_, err := lnx.Run(testConfig(dir), "true")
	require.Error(t, err)
	assert.Contains(t, err.Error(), "locked by another instance")
	assert.Contains(t, err.Error(), "pid")

	wg.Wait()
}

func TestRun_StaleLockRecovery(t *testing.T) {
	t.Parallel()
	dir := setupTestDir(t)
	cfg := testConfig(dir)

	// Simulate a crashed process: create stale lock and pid files.
	lockPath := cfg.RootfsPath + ".lock"
	pidPath := cfg.RootfsPath + ".pid"
	os.WriteFile(lockPath, nil, 0644)
	os.WriteFile(pidPath, []byte("99999\n"), 0644)

	// Should succeed — flock is NOT held, just stale files.
	exitCode, err := lnx.Run(cfg, "true")
	require.NoError(t, err)
	assert.Equal(t, 0, exitCode)
}

func TestRun_PidfileCreatedAndCleaned(t *testing.T) {
	t.Parallel()
	dir := setupTestDir(t)
	cfg := testConfig(dir)
	pidPath := cfg.RootfsPath + ".pid"
	lockPath := cfg.RootfsPath + ".lock"

	exitCode, err := lnx.Run(cfg, "true")
	require.NoError(t, err)
	assert.Equal(t, 0, exitCode)

	_, err = os.Stat(pidPath)
	assert.True(t, os.IsNotExist(err), "pidfile should be removed after run")

	_, err = os.Stat(lockPath)
	assert.True(t, os.IsNotExist(err), "lock file should be removed after run")
}
