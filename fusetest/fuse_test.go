//go:build linux

// Package fusetest contains filesystem tests that run inside an lnx VM
// on a FUSE-cached 9P mount. They exercise the lazyCacheFS behavior:
// cache-first reads, write-to-cache semantics, directory operations,
// permission preservation, symlink handling, and kernel cache behavior.
//
// Cross-compile: CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go test -c -o fusetest.test ./fusetest
// Run inside VM: lnx ./fusetest.test -test.v
package fusetest

import (
	"crypto/md5"
	"fmt"
	"math/rand"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"
)

// testDir returns the sync share directory to test against.
// Set via FUSETEST_DIR env var, or auto-detected from /proc/mounts.
func testDir(t *testing.T) string {
	t.Helper()
	if d := os.Getenv("FUSETEST_DIR"); d != "" {
		return d
	}
	// Find a lnx-sync FUSE mount.
	data, err := os.ReadFile("/proc/mounts")
	if err != nil {
		t.Skip("not inside lnx VM: cannot read /proc/mounts")
	}
	for _, line := range strings.Split(string(data), "\n") {
		fields := strings.Fields(line)
		if len(fields) >= 3 && fields[0] == "lnx-sync" && strings.HasPrefix(fields[2], "fuse.") {
			// Only use sync share mounts (fuse.sync*). Skip home and CWD.
			if !strings.HasPrefix(fields[2], "fuse.sync") {
				continue
			}
			return fields[1]
		}
	}
	t.Skip("no lnx-sync FUSE mount found")
	return ""
}

// sub creates a unique subdirectory for a test to work in.
func sub(t *testing.T, base string) string {
	t.Helper()
	dir := filepath.Join(base, fmt.Sprintf("t-%d", time.Now().UnixNano()))
	if err := os.MkdirAll(dir, 0755); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(dir) })
	return dir
}

func TestReadFile(t *testing.T) {
	dir := testDir(t)
	// fixture.txt must be pre-placed by the host test harness.
	data, err := os.ReadFile(filepath.Join(dir, "fixture.txt"))
	if err != nil {
		t.Fatal(err)
	}
	if string(data) != "hello from host\n" {
		t.Fatalf("got %q, want %q", data, "hello from host\n")
	}
}

func TestReadFileTwice(t *testing.T) {
	dir := testDir(t)
	path := filepath.Join(dir, "fixture.txt")

	// First read — hydrates from 9P lower into ext4 cache.
	start1 := time.Now()
	d1, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	dur1 := time.Since(start1)

	// Second read — should come from ext4 cache (faster).
	start2 := time.Now()
	d2, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	dur2 := time.Since(start2)

	if string(d1) != string(d2) {
		t.Fatalf("content mismatch: %q vs %q", d1, d2)
	}
	t.Logf("read1=%v read2=%v", dur1, dur2)
}

func TestWriteFile(t *testing.T) {
	dir := sub(t, testDir(t))
	path := filepath.Join(dir, "guest-created.txt")

	if err := os.WriteFile(path, []byte("from guest"), 0644); err != nil {
		t.Fatal(err)
	}

	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if string(data) != "from guest" {
		t.Fatalf("got %q, want %q", data, "from guest")
	}
}

func TestStatPermissions(t *testing.T) {
	dir := testDir(t)
	// perm600.txt must be pre-placed by host with mode 0600.
	info, err := os.Stat(filepath.Join(dir, "perm600.txt"))
	if err != nil {
		t.Fatal(err)
	}
	perm := info.Mode().Perm()
	if perm != 0600 {
		t.Fatalf("got %o, want 600", perm)
	}
}

func TestMkdirAndList(t *testing.T) {
	dir := sub(t, testDir(t))
	subdir := filepath.Join(dir, "a", "b")
	if err := os.MkdirAll(subdir, 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(subdir, "f.txt"), []byte("nested"), 0644); err != nil {
		t.Fatal(err)
	}

	entries, err := os.ReadDir(subdir)
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) != 1 || entries[0].Name() != "f.txt" {
		t.Fatalf("unexpected entries: %v", entries)
	}
}

func TestSymlink(t *testing.T) {
	dir := testDir(t)
	// link.txt -> fixture.txt must be pre-placed by host.
	target, err := os.Readlink(filepath.Join(dir, "link.txt"))
	if err != nil {
		t.Fatal(err)
	}
	if target != "fixture.txt" {
		t.Fatalf("got target %q, want %q", target, "fixture.txt")
	}
	data, err := os.ReadFile(filepath.Join(dir, "link.txt"))
	if err != nil {
		t.Fatal(err)
	}
	if string(data) != "hello from host\n" {
		t.Fatalf("got %q through symlink", data)
	}
}

func TestRename(t *testing.T) {
	dir := sub(t, testDir(t))
	old := filepath.Join(dir, "old.txt")
	neu := filepath.Join(dir, "new.txt")

	if err := os.WriteFile(old, []byte("rename me"), 0644); err != nil {
		t.Fatalf("write old: %v", err)
	}
	// Verify old exists before rename.
	if _, err := os.Stat(old); err != nil {
		t.Fatalf("stat old before rename: %v", err)
	}
	if err := os.Rename(old, neu); err != nil {
		t.Fatalf("rename: %v", err)
	}
	// List parent dir to see what the FUSE reports.
	entries, _ := os.ReadDir(dir)
	var names []string
	for _, e := range entries {
		names = append(names, e.Name())
	}
	t.Logf("after rename, parent dir contains: %v", names)
	// Verify new exists after rename.
	if _, err := os.Stat(neu); err != nil {
		t.Fatalf("stat new after rename: %v (dir contents: %v)", err, names)
	}
	data, err := os.ReadFile(neu)
	if err != nil {
		t.Fatalf("read new: %v", err)
	}
	if string(data) != "rename me" {
		t.Fatalf("got %q", data)
	}
}

func TestUnlink(t *testing.T) {
	dir := sub(t, testDir(t))
	path := filepath.Join(dir, "removeme.txt")

	if err := os.WriteFile(path, []byte("gone"), 0644); err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(path); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Fatalf("file still exists after unlink")
	}
}

func TestReaddir(t *testing.T) {
	dir := testDir(t)
	entries, err := os.ReadDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	names := map[string]bool{}
	for _, e := range entries {
		names[e.Name()] = true
	}
	for _, want := range []string{"fixture.txt", "perm600.txt", "link.txt"} {
		if !names[want] {
			t.Errorf("missing %q in readdir (got %v)", want, names)
		}
	}
}

func TestLargeFile(t *testing.T) {
	dir := testDir(t)
	// large.bin must be pre-placed by host (1MB random data).
	data, err := os.ReadFile(filepath.Join(dir, "large.bin"))
	if err != nil {
		t.Fatal(err)
	}
	if len(data) != 1<<20 {
		t.Fatalf("got %d bytes, want %d", len(data), 1<<20)
	}
	// Verify md5 matches host-placed checksum.
	hostMD5, err := os.ReadFile(filepath.Join(dir, "large.bin.md5"))
	if err != nil {
		t.Fatal(err)
	}
	got := fmt.Sprintf("%x", md5.Sum(data))
	if got != strings.TrimSpace(string(hostMD5)) {
		t.Fatalf("md5 mismatch: got %s, want %s", got, strings.TrimSpace(string(hostMD5)))
	}
}

func TestConcurrentReads(t *testing.T) {
	dir := testDir(t)
	path := filepath.Join(dir, "fixture.txt")

	var wg sync.WaitGroup
	errs := make(chan error, 20)
	for i := 0; i < 20; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			data, err := os.ReadFile(path)
			if err != nil {
				errs <- err
				return
			}
			if string(data) != "hello from host\n" {
				errs <- fmt.Errorf("got %q", data)
			}
		}()
	}
	wg.Wait()
	close(errs)
	for err := range errs {
		t.Error(err)
	}
}

func TestWriteLargeFile(t *testing.T) {
	dir := sub(t, testDir(t))
	path := filepath.Join(dir, "written.bin")

	data := make([]byte, 1<<20)
	rand.Read(data)
	if err := os.WriteFile(path, data, 0644); err != nil {
		t.Fatal(err)
	}

	read, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if md5.Sum(read) != md5.Sum(data) {
		t.Fatal("md5 mismatch after write+read")
	}
}

func TestFilenameWithSpaces(t *testing.T) {
	dir := testDir(t)
	// "hello world.txt" must be pre-placed by host.
	data, err := os.ReadFile(filepath.Join(dir, "hello world.txt"))
	if err != nil {
		t.Fatal(err)
	}
	if string(data) != "spaces ok\n" {
		t.Fatalf("got %q", data)
	}
}
