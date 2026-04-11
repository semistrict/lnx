//go:build darwin && integration

package lnx_test

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestMktemp_DefaultCreatesFileUnderTmp(t *testing.T) {
	bin, err := filepath.Abs("lnx")
	require.NoError(t, err)
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("skipping: repo lnx binary not found at %s", bin)
	}

	out := runCLISuccess(t, bin, "mktemp")
	path := strings.TrimSpace(out)
	assert.True(t, strings.HasPrefix(path, "/tmp/lnx."), "path should start with /tmp/lnx., got %q", path)
	_, err = os.Stat(path)
	require.NoError(t, err, "file should exist")
	t.Cleanup(func() { os.Remove(path) })
}

func TestMktemp_DirectoryFlag(t *testing.T) {
	bin, err := filepath.Abs("lnx")
	require.NoError(t, err)
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("skipping: repo lnx binary not found at %s", bin)
	}

	out := runCLISuccess(t, bin, "mktemp", "-d")
	path := strings.TrimSpace(out)
	assert.True(t, strings.HasPrefix(path, "/tmp/lnx."), "path should start with /tmp/lnx., got %q", path)
	info, err := os.Stat(path)
	require.NoError(t, err, "directory should exist")
	assert.True(t, info.IsDir(), "should be a directory")
	t.Cleanup(func() { os.RemoveAll(path) })
}

func TestMktemp_CustomTemplate(t *testing.T) {
	bin, err := filepath.Abs("lnx")
	require.NoError(t, err)
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("skipping: repo lnx binary not found at %s", bin)
	}

	out := runCLISuccess(t, bin, "mktemp", "myapp.XXXXXX")
	path := strings.TrimSpace(out)
	assert.True(t, strings.HasPrefix(path, "/tmp/myapp."), "path should start with /tmp/myapp., got %q", path)
	_, err = os.Stat(path)
	require.NoError(t, err, "file should exist")
	t.Cleanup(func() { os.Remove(path) })
}

func TestMktemp_PrefixFlag(t *testing.T) {
	bin, err := filepath.Abs("lnx")
	require.NoError(t, err)
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("skipping: repo lnx binary not found at %s", bin)
	}

	out := runCLISuccess(t, bin, "mktemp", "-t", "build")
	path := strings.TrimSpace(out)
	assert.True(t, strings.HasPrefix(path, "/tmp/build."), "path should start with /tmp/build., got %q", path)
	_, err = os.Stat(path)
	require.NoError(t, err, "file should exist")
	t.Cleanup(func() { os.Remove(path) })
}

func TestMktemp_CustomTmpdir(t *testing.T) {
	bin, err := filepath.Abs("lnx")
	require.NoError(t, err)
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("skipping: repo lnx binary not found at %s", bin)
	}

	dir := t.TempDir()
	out := runCLISuccess(t, bin, "mktemp", "-p", dir)
	path := strings.TrimSpace(out)
	assert.True(t, strings.HasPrefix(path, dir+"/lnx."), "path should be under %s, got %q", dir, path)
	_, err = os.Stat(path)
	require.NoError(t, err, "file should exist")
}

func TestMktemp_DryRunDoesNotCreate(t *testing.T) {
	bin, err := filepath.Abs("lnx")
	require.NoError(t, err)
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("skipping: repo lnx binary not found at %s", bin)
	}

	out := runCLISuccess(t, bin, "mktemp", "-u")
	path := strings.TrimSpace(out)
	assert.True(t, strings.HasPrefix(path, "/tmp/lnx."), "path should start with /tmp/lnx., got %q", path)
	_, err = os.Stat(path)
	assert.True(t, os.IsNotExist(err), "file should not exist in dry-run mode")
}

func TestMktemp_TemplateWithDirectory(t *testing.T) {
	bin, err := filepath.Abs("lnx")
	require.NoError(t, err)
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("skipping: repo lnx binary not found at %s", bin)
	}

	dir := t.TempDir()
	out := runCLISuccess(t, bin, "mktemp", filepath.Join(dir, "test.XXXXXX"))
	path := strings.TrimSpace(out)
	assert.True(t, strings.HasPrefix(path, dir+"/test."), "path should be under %s with prefix test., got %q", dir, path)
	_, err = os.Stat(path)
	require.NoError(t, err, "file should exist")
}

func TestMktemp_PathWorksOnGuest(t *testing.T) {
	bin, err := filepath.Abs("lnx")
	require.NoError(t, err)
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("skipping: repo lnx binary not found at %s", bin)
	}

	inst := "test-mktemp-guest"
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	// Create a temp file on the host.
	out := runCLISuccess(t, bin, "mktemp")
	hostPath := strings.TrimSpace(out)
	t.Cleanup(func() { os.Remove(hostPath) })

	// The path should start with /tmp/ which also exists in the guest.
	assert.True(t, strings.HasPrefix(hostPath, "/tmp/"))

	// Verify the guest can create a file at the same structural path.
	guestOut := runCLISuccess(t, bin, "--instance", inst,
		"sh", "-c", "touch "+hostPath+" && echo OK")
	assert.Contains(t, guestOut, "OK")

	// Clean up.
	runCLISuccess(t, bin, "--instance", inst, "stop", "--shutdown")
}
