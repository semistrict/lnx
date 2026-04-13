//go:build darwin && integration

package lnx_test

import (
	"crypto/md5"
	"encoding/hex"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

// TestSyncShare_FileAccessible verifies that a file in a sync share is readable
// inside the VM at the same absolute path via the FUSE lazy-cache mount.
func TestSyncShare_FileAccessible(t *testing.T) {
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-%d", time.Now().UnixNano())
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	shareDir := t.TempDir()
	require.NoError(t, os.WriteFile(filepath.Join(shareDir, "hello.txt"), []byte("sync-ok"), 0644))

	// Add the sync share.
	out, err := runCLI(bin, "--instance", inst, "sync", "add", shareDir)
	require.NoError(t, err, "sync add failed: %s", out)

	// Read the file from inside the VM.
	out = runCLISuccess(t, bin, "--instance", inst, "--ephemeral", "cat", filepath.Join(shareDir, "hello.txt"))
	assert.Contains(t, out, "sync-ok")
}

// TestSyncShare_DirectoryListing verifies that directory listing works on the FUSE mount.
func TestSyncShare_DirectoryListing(t *testing.T) {
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-ls-%d", time.Now().UnixNano())
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	shareDir := t.TempDir()
	require.NoError(t, os.WriteFile(filepath.Join(shareDir, "a.txt"), []byte("a"), 0644))
	require.NoError(t, os.WriteFile(filepath.Join(shareDir, "b.txt"), []byte("b"), 0644))

	out, err := runCLI(bin, "--instance", inst, "sync", "add", shareDir)
	require.NoError(t, err, "sync add failed: %s", out)

	// ls should show both files.
	out = runCLISuccess(t, bin, "--instance", inst, "--ephemeral", "ls", shareDir)
	assert.Contains(t, out, "a.txt")
	assert.Contains(t, out, "b.txt")
}

// TestSyncShare_GuestWriteStaysInCache verifies that writing inside the VM
// does NOT appear on the host (lower virtiofs is read-only; writes go to ext4 cache).
func TestSyncShare_GuestWriteStaysInCache(t *testing.T) {
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-write-%d", time.Now().UnixNano())
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	shareDir := t.TempDir()
	guestFile := filepath.Join(shareDir, "from_guest.txt")

	out, err := runCLI(bin, "--instance", inst, "sync", "add", shareDir)
	require.NoError(t, err, "sync add failed: %s", out)

	// Write from inside the VM.
	runCLISuccess(t, bin, "--instance", inst, "--ephemeral", "sh", "-c", "echo cache-write > "+guestFile)

	// The file must NOT be visible on the host (lower virtiofs is read-only).
	_, err = os.Stat(guestFile)
	assert.True(t, os.IsNotExist(err), "guest write leaked to host: %s should not exist on host", guestFile)
}

// TestSyncShare_SubdirectoryAccessible verifies that nested paths work.
func TestSyncShare_SubdirectoryAccessible(t *testing.T) {
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-sub-%d", time.Now().UnixNano())
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	shareDir := t.TempDir()
	subDir := filepath.Join(shareDir, "sub", "nested")
	require.NoError(t, os.MkdirAll(subDir, 0755))
	require.NoError(t, os.WriteFile(filepath.Join(subDir, "deep.txt"), []byte("deep-ok"), 0644))

	out, err := runCLI(bin, "--instance", inst, "sync", "add", shareDir)
	require.NoError(t, err, "sync add failed: %s", out)

	out = runCLISuccess(t, bin, "--instance", inst, "--ephemeral", "cat", filepath.Join(subDir, "deep.txt"))
	assert.Contains(t, out, "deep-ok")
}

// TestSyncShare_ZeroSyncSharesRegression verifies that booting with no sync shares
// configured continues to work normally.
func TestSyncShare_ZeroSyncSharesRegression(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-zero-%d", time.Now().UnixNano())
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	// No sync shares added — boot and run a simple command.
	out := runCLISuccess(t, bin, "--instance", inst, "--ephemeral", "echo", "zero-ok")
	assert.Contains(t, out, "zero-ok")
}

// TestSyncShare_BackgroundRefresh verifies that the background refresh goroutine
// re-hydrates cached files when the lower (host) copy is updated.
func TestSyncShare_BackgroundRefresh(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-refresh-%d", time.Now().UnixNano())
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	shareDir := t.TempDir()
	hostFile := filepath.Join(shareDir, "watched.txt")
	require.NoError(t, os.WriteFile(hostFile, []byte("original"), 0644))

	out, err := runCLI(bin, "--instance", inst, "sync", "add", shareDir)
	require.NoError(t, err, "sync add failed: %s", out)

	// Guest: read file (hydrate into cache), signal ready, sleep 8s, read again.
	// The 8s sleep ensures at least one 5s refresh cycle fires after the host write.
	script := `cat ` + hostFile + `; echo HYDRATED; sleep 8; cat ` + hostFile + `; echo REFRESH_DONE`
	cmd, lines, stderr, done := startStreamingCLI(t, bin, "--instance", inst, "--ephemeral", "sh", "-c", script)
	t.Cleanup(func() { cleanupStreamingCLI(t, cmd, done, stderr) })

	waitForCLIOutput(t, lines, "HYDRATED", 30*time.Second, stderr)

	// Update the host file. Set mtime 2s in the future to guarantee the mtime
	// comparison (second resolution) detects staleness even under fast machines.
	require.NoError(t, os.WriteFile(hostFile, []byte("refreshed"), 0644))
	future := time.Now().Add(2 * time.Second)
	require.NoError(t, os.Chtimes(hostFile, future, future))

	// Background refresh (every 5s) must pick up the change within the 8s sleep.
	waitForCLIOutput(t, lines, "refreshed", 15*time.Second, stderr)

	select {
	case err := <-done:
		require.NoError(t, err, "process failed: %s", stderr.String())
	case <-time.After(20 * time.Second):
		t.Fatal("process did not exit in time")
	}
}

// TestSyncShare_OpenTimeReHydration verifies that Open() re-hydrates a cached file
// when the lower copy has a newer mtime, without waiting for the background refresh.
func TestSyncShare_OpenTimeReHydration(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-rehydrate-%d", time.Now().UnixNano())
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	shareDir := t.TempDir()
	hostFile := filepath.Join(shareDir, "data.txt")
	require.NoError(t, os.WriteFile(hostFile, []byte("v1"), 0644))

	out, err := runCLI(bin, "--instance", inst, "sync", "add", shareDir)
	require.NoError(t, err, "sync add failed: %s", out)

	// Guest: read file (hydrate), signal, sleep 2s (well under 5s refresh interval),
	// then read again. Re-hydration must come from Open(), not the background refresher.
	script := `cat ` + hostFile + `; echo HYDRATED; sleep 2; cat ` + hostFile + `; echo REHYDRATE_DONE`
	cmd, lines, stderr, done := startStreamingCLI(t, bin, "--instance", inst, "--ephemeral", "sh", "-c", script)
	t.Cleanup(func() { cleanupStreamingCLI(t, cmd, done, stderr) })

	waitForCLIOutput(t, lines, "HYDRATED", 30*time.Second, stderr)

	// Write new content with a future mtime so Open() detects staleness immediately.
	require.NoError(t, os.WriteFile(hostFile, []byte("v2"), 0644))
	future := time.Now().Add(2 * time.Second)
	require.NoError(t, os.Chtimes(hostFile, future, future))

	waitForCLIOutput(t, lines, "v2", 10*time.Second, stderr)

	select {
	case err := <-done:
		require.NoError(t, err, "process failed: %s", stderr.String())
	case <-time.After(15 * time.Second):
		t.Fatal("process did not exit in time")
	}
}

// TestSyncShare_MultipleSyncShares verifies that two distinct sync shares are both
// mounted and accessible inside the VM.
func TestSyncShare_MultipleSyncShares(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-multi-%d", time.Now().UnixNano())
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	dir1 := t.TempDir()
	dir2 := t.TempDir()
	require.NoError(t, os.WriteFile(filepath.Join(dir1, "one.txt"), []byte("share-one"), 0644))
	require.NoError(t, os.WriteFile(filepath.Join(dir2, "two.txt"), []byte("share-two"), 0644))

	out, err := runCLI(bin, "--instance", inst, "sync", "add", dir1)
	require.NoError(t, err, "sync add 1 failed: %s", out)
	out, err = runCLI(bin, "--instance", inst, "sync", "add", dir2)
	require.NoError(t, err, "sync add 2 failed: %s", out)

	script := `cat ` + filepath.Join(dir1, "one.txt") + ` && cat ` + filepath.Join(dir2, "two.txt")
	out = runCLISuccess(t, bin, "--instance", inst, "--ephemeral", "sh", "-c", script)
	assert.Contains(t, out, "share-one")
	assert.Contains(t, out, "share-two")
}

// TestSync_RemoveCommand verifies that `lnx sync remove` removes a share from the
// persisted list without touching the host directory.
func TestSync_RemoveCommand(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-remove-%d", time.Now().UnixNano())
	home, _ := os.UserHomeDir()
	instDir := filepath.Join(home, ".lnx", "instances", inst)
	require.NoError(t, os.MkdirAll(instDir, 0755))
	t.Cleanup(func() { os.RemoveAll(instDir) })

	shareDir := t.TempDir()

	out, err := runCLI(bin, "--instance", inst, "sync", "add", shareDir)
	require.NoError(t, err, "sync add failed: %s", out)

	listOut := runCLISuccess(t, bin, "--instance", inst, "sync", "list")
	assert.Contains(t, listOut, shareDir)

	out, err = runCLI(bin, "--instance", inst, "sync", "remove", shareDir)
	require.NoError(t, err, "sync remove failed: %s", out)

	listOut = runCLISuccess(t, bin, "--instance", inst, "sync", "list")
	assert.NotContains(t, listOut, shareDir)

	// The host directory must still exist.
	_, err = os.Stat(shareDir)
	require.NoError(t, err, "sync remove must not delete the host directory")
}

// TestSync_ListCommand verifies `lnx sync list` for both the empty and populated cases.
func TestSync_ListCommand(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-list-%d", time.Now().UnixNano())
	home, _ := os.UserHomeDir()
	instDir := filepath.Join(home, ".lnx", "instances", inst)
	require.NoError(t, os.MkdirAll(instDir, 0755))
	t.Cleanup(func() { os.RemoveAll(instDir) })

	// Empty list.
	out := runCLISuccess(t, bin, "--instance", inst, "sync", "list")
	assert.Contains(t, out, "no sync shares")

	// Add two shares and list.
	dir1 := t.TempDir()
	dir2 := t.TempDir()
	runCLISuccess(t, bin, "--instance", inst, "sync", "add", dir1)
	runCLISuccess(t, bin, "--instance", inst, "sync", "add", dir2)

	out = runCLISuccess(t, bin, "--instance", inst, "sync", "list")
	assert.Contains(t, out, dir1)
	assert.Contains(t, out, dir2)
}

// TestSyncShare_GuestCreate verifies that a file created by the guest inside the FUSE
// mount is readable in the same session (cache write).
func TestSyncShare_GuestCreate(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-gcreate-%d", time.Now().UnixNano())
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	shareDir := t.TempDir()
	out, err := runCLI(bin, "--instance", inst, "sync", "add", shareDir)
	require.NoError(t, err, "sync add failed: %s", out)

	guestFile := filepath.Join(shareDir, "from_guest.txt")
	script := `echo guest-created > ` + guestFile + ` && cat ` + guestFile
	out = runCLISuccess(t, bin, "--instance", inst, "--ephemeral", "sh", "-c", script)
	assert.Contains(t, out, "guest-created")
}

// TestSyncShare_GuestMkdir verifies that the guest can create nested directories and
// files inside the FUSE mount.
func TestSyncShare_GuestMkdir(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-gdir-%d", time.Now().UnixNano())
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	shareDir := t.TempDir()
	out, err := runCLI(bin, "--instance", inst, "sync", "add", shareDir)
	require.NoError(t, err, "sync add failed: %s", out)

	subDir := filepath.Join(shareDir, "sub", "nested")
	nestedFile := filepath.Join(subDir, "deep.txt")
	script := `mkdir -p ` + subDir + ` && echo deep-content > ` + nestedFile + ` && cat ` + nestedFile
	out = runCLISuccess(t, bin, "--instance", inst, "--ephemeral", "sh", "-c", script)
	assert.Contains(t, out, "deep-content")
}

// TestSyncShare_SymlinkInLower verifies that a symlink present in the host (lower)
// directory is readable through the FUSE mount.
func TestSyncShare_SymlinkInLower(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-symlink-%d", time.Now().UnixNano())
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	shareDir := t.TempDir()
	require.NoError(t, os.WriteFile(filepath.Join(shareDir, "target.txt"), []byte("link-ok"), 0644))
	require.NoError(t, os.Symlink("target.txt", filepath.Join(shareDir, "link.txt")))

	out, err := runCLI(bin, "--instance", inst, "sync", "add", shareDir)
	require.NoError(t, err, "sync add failed: %s", out)

	linkPath := filepath.Join(shareDir, "link.txt")
	script := `readlink ` + linkPath + ` && cat ` + linkPath
	out = runCLISuccess(t, bin, "--instance", inst, "--ephemeral", "sh", "-c", script)
	assert.Contains(t, out, "target.txt")
	assert.Contains(t, out, "link-ok")
}

// TestSyncShare_FilePermissions verifies that file permissions from the lower (host)
// directory are preserved after hydration into the ext4 cache.
func TestSyncShare_FilePermissions(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-perms-%d", time.Now().UnixNano())
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	shareDir := t.TempDir()
	secretFile := filepath.Join(shareDir, "secret.txt")
	require.NoError(t, os.WriteFile(secretFile, []byte("secret"), 0600))

	out, err := runCLI(bin, "--instance", inst, "sync", "add", shareDir)
	require.NoError(t, err, "sync add failed: %s", out)

	// stat -c %a prints octal permissions without leading zero.
	out = runCLISuccess(t, bin, "--instance", inst, "--ephemeral", "stat", "-c", "%a", secretFile)
	assert.Equal(t, "600\n", out)
}

// TestSync_AddIdempotent verifies that calling `lnx sync add` twice with the same
// path results in the path appearing exactly once in the list.
func TestSync_AddIdempotent(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-idem-%d", time.Now().UnixNano())
	home, _ := os.UserHomeDir()
	instDir := filepath.Join(home, ".lnx", "instances", inst)
	require.NoError(t, os.MkdirAll(instDir, 0755))
	t.Cleanup(func() { os.RemoveAll(instDir) })

	shareDir := t.TempDir()

	runCLISuccess(t, bin, "--instance", inst, "sync", "add", shareDir)
	// Second add with the same path should be a no-op.
	runCLISuccess(t, bin, "--instance", inst, "sync", "add", shareDir)

	out := runCLISuccess(t, bin, "--instance", inst, "sync", "list")
	assert.Equal(t, 1, strings.Count(out, shareDir), "path should appear exactly once in list")
}

// TestSync_AddRejectsNonDirectory verifies that `lnx sync add` returns an error when
// given a path that is a regular file, not a directory.
func TestSync_AddRejectsNonDirectory(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-nodir-%d", time.Now().UnixNano())
	home, _ := os.UserHomeDir()
	instDir := filepath.Join(home, ".lnx", "instances", inst)
	require.NoError(t, os.MkdirAll(instDir, 0755))
	t.Cleanup(func() { os.RemoveAll(instDir) })

	regularFile := filepath.Join(t.TempDir(), "notadir.txt")
	require.NoError(t, os.WriteFile(regularFile, []byte("x"), 0644))

	out, err := runCLI(bin, "--instance", inst, "sync", "add", regularFile)
	require.Error(t, err, "expected error for non-directory path")
	assert.Contains(t, out, "not a directory")
}

// TestSync_AddRejectsNonExistent verifies that `lnx sync add` returns an error when
// the given path does not exist.
func TestSync_AddRejectsNonExistent(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-nonexist-%d", time.Now().UnixNano())
	home, _ := os.UserHomeDir()
	instDir := filepath.Join(home, ".lnx", "instances", inst)
	require.NoError(t, os.MkdirAll(instDir, 0755))
	t.Cleanup(func() { os.RemoveAll(instDir) })

	out, err := runCLI(bin, "--instance", inst, "sync", "add", "/tmp/does-not-exist-lnx-test-12345")
	require.Error(t, err, "expected error for non-existent path")
	assert.Contains(t, out, "no such file or directory")
}

// TestSyncShare_GuestUnlink verifies that a guest `rm` succeeds (removes from cache)
// and does not delete the original file on the host.
func TestSyncShare_GuestUnlink(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-unlink-%d", time.Now().UnixNano())
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	shareDir := t.TempDir()
	hostFile := filepath.Join(shareDir, "ephemeral.txt")
	require.NoError(t, os.WriteFile(hostFile, []byte("removeme"), 0644))

	out, err := runCLI(bin, "--instance", inst, "sync", "add", shareDir)
	require.NoError(t, err, "sync add failed: %s", out)

	// Read the file first (hydrate into cache), then remove it.
	script := `cat ` + hostFile + ` && rm ` + hostFile + ` && echo REMOVED`
	out = runCLISuccess(t, bin, "--instance", inst, "--ephemeral", "sh", "-c", script)
	assert.Contains(t, out, "removeme")
	assert.Contains(t, out, "REMOVED")

	// The host file must still exist — Unlink only removes from cache.
	_, err = os.Stat(hostFile)
	assert.NoError(t, err, "host file must survive a guest unlink")
}

// TestSyncShare_GuestRename verifies that the guest can rename a file within the FUSE
// mount and access it under the new name.
func TestSyncShare_GuestRename(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-rename-%d", time.Now().UnixNano())
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	shareDir := t.TempDir()
	require.NoError(t, os.WriteFile(filepath.Join(shareDir, "old.txt"), []byte("rename-me"), 0644))

	out, err := runCLI(bin, "--instance", inst, "sync", "add", shareDir)
	require.NoError(t, err, "sync add failed: %s", out)

	oldPath := filepath.Join(shareDir, "old.txt")
	newPath := filepath.Join(shareDir, "new.txt")
	// Read old (hydrate), rename, then read under the new name.
	script := `cat ` + oldPath + ` && mv ` + oldPath + ` ` + newPath + ` && cat ` + newPath + ` && echo RENAMED`
	out = runCLISuccess(t, bin, "--instance", inst, "--ephemeral", "sh", "-c", script)
	assert.Contains(t, out, "rename-me")
	assert.Contains(t, out, "RENAMED")
}

// TestSyncShare_ReaddirCacheInvalidation verifies that when the host adds a file to
// a shared directory, a subsequent guest `ls` sees the new file (the per-directory
// listing cache is invalidated by the changed lower mtime).
func TestSyncShare_ReaddirCacheInvalidation(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-readdir-%d", time.Now().UnixNano())
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	shareDir := t.TempDir()
	require.NoError(t, os.WriteFile(filepath.Join(shareDir, "existing.txt"), []byte("x"), 0644))

	out, err := runCLI(bin, "--instance", inst, "sync", "add", shareDir)
	require.NoError(t, err, "sync add failed: %s", out)

	// Guest: list dir, signal done, sleep 3s, list again.
	script := `ls ` + shareDir + `; echo DONE_LS1; sleep 3; ls ` + shareDir + `; echo DONE_LS2`
	cmd, lines, stderr, done := startStreamingCLI(t, bin, "--instance", inst, "--ephemeral", "sh", "-c", script)
	t.Cleanup(func() { cleanupStreamingCLI(t, cmd, done, stderr) })

	waitForCLIOutput(t, lines, "DONE_LS1", 30*time.Second, stderr)

	// Add a new file and bump the dir mtime so the FUSE cache invalidates.
	require.NoError(t, os.WriteFile(filepath.Join(shareDir, "newfile.txt"), []byte("new"), 0644))
	future := time.Now().Add(2 * time.Second)
	require.NoError(t, os.Chtimes(shareDir, future, future))

	// Second ls must include the new file.
	waitForCLIOutput(t, lines, "newfile.txt", 10*time.Second, stderr)

	select {
	case err := <-done:
		require.NoError(t, err, "process failed: %s", stderr.String())
	case <-time.After(15 * time.Second):
		t.Fatal("process did not exit in time")
	}
}

// TestSyncShare_EmptyDirectory verifies that an empty sync share directory can be
// listed inside the VM without error.
func TestSyncShare_EmptyDirectory(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-empty-%d", time.Now().UnixNano())
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	shareDir := t.TempDir() // intentionally empty

	out, err := runCLI(bin, "--instance", inst, "sync", "add", shareDir)
	require.NoError(t, err, "sync add failed: %s", out)

	// ls on empty FUSE mount should succeed with no files listed.
	out = runCLISuccess(t, bin, "--instance", inst, "--ephemeral", "sh", "-c",
		`ls `+shareDir+` && echo EMPTY_OK`)
	assert.Contains(t, out, "EMPTY_OK")
}

// TestSyncShare_BinaryFileIntegrity verifies that binary files survive the
// lower→cache hydration without corruption by comparing md5 checksums.
func TestSyncShare_BinaryFileIntegrity(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-binary-%d", time.Now().UnixNano())
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	shareDir := t.TempDir()
	binaryFile := filepath.Join(shareDir, "data.bin")

	// Create a file containing all 256 byte values.
	content := make([]byte, 256)
	for i := range content {
		content[i] = byte(i)
	}
	require.NoError(t, os.WriteFile(binaryFile, content, 0644))

	sum := md5.Sum(content)
	hostMD5 := hex.EncodeToString(sum[:])

	out, err := runCLI(bin, "--instance", inst, "sync", "add", shareDir)
	require.NoError(t, err, "sync add failed: %s", out)

	// md5sum output: "<hash>  <filename>"
	out = runCLISuccess(t, bin, "--instance", inst, "--ephemeral", "md5sum", binaryFile)
	assert.Contains(t, out, hostMD5, "guest md5 must match host md5")
}

// TestSyncShare_FilenameWithSpaces verifies that files whose names contain spaces
// are accessible through the FUSE mount.
func TestSyncShare_FilenameWithSpaces(t *testing.T) {
	t.Parallel()
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-sync-spaces-%d", time.Now().UnixNano())
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	shareDir := t.TempDir()
	spacedFile := filepath.Join(shareDir, "hello world.txt")
	require.NoError(t, os.WriteFile(spacedFile, []byte("spaces-ok"), 0644))

	out, err := runCLI(bin, "--instance", inst, "sync", "add", shareDir)
	require.NoError(t, err, "sync add failed: %s", out)

	out = runCLISuccess(t, bin, "--instance", inst, "--ephemeral", "cat", spacedFile)
	assert.Contains(t, out, "spaces-ok")
}
