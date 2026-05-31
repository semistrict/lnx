//go:build darwin && integration

package lnx_test

import (
	"crypto/md5"
	"fmt"
	"math/rand"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

// TestFuseFilesystem cross-compiles the fusetest binary, places test fixtures
// in a sync share, and runs the tests inside an lnx VM on the actual FUSE mount.
func TestFuseFilesystem(t *testing.T) {
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := fmt.Sprintf("test-fusetest-%d", time.Now().UnixNano())
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	// --- Cross-compile the guest test binary ---
	testBin := filepath.Join(t.TempDir(), "fusetest.test")
	build := exec.Command("go", "test", "-c", "-o", testBin, "./fusetest")
	build.Env = append(os.Environ(), "CGO_ENABLED=0", "GOOS=linux", "GOARCH=arm64")
	out, err := build.CombinedOutput()
	require.NoError(t, err, "cross-compile fusetest: %s", out)

	// --- Create sync share with test fixtures ---
	shareDir := t.TempDir()

	require.NoError(t, os.WriteFile(filepath.Join(shareDir, "fixture.txt"), []byte("hello from host\n"), 0644))
	require.NoError(t, os.WriteFile(filepath.Join(shareDir, "perm600.txt"), []byte("secret"), 0600))
	require.NoError(t, os.Symlink("fixture.txt", filepath.Join(shareDir, "link.txt")))
	require.NoError(t, os.WriteFile(filepath.Join(shareDir, "hello world.txt"), []byte("spaces ok\n"), 0644))

	// 1MB random file + its md5.
	largeData := make([]byte, 1<<20)
	rand.Read(largeData)
	require.NoError(t, os.WriteFile(filepath.Join(shareDir, "large.bin"), largeData, 0644))
	largeMD5 := fmt.Sprintf("%x", md5.Sum(largeData))
	require.NoError(t, os.WriteFile(filepath.Join(shareDir, "large.bin.md5"), []byte(largeMD5+"\n"), 0644))

	// Copy the test binary into the share so the guest can execute it.
	testBinData, err := os.ReadFile(testBin)
	require.NoError(t, err)
	guestBin := filepath.Join(shareDir, "fusetest.test")
	require.NoError(t, os.WriteFile(guestBin, testBinData, 0755))

	// Add sync share.
	addOut, err := runCLI(bin, "--instance", inst, "sync", "add", shareDir)
	require.NoError(t, err, "sync add: %s", addOut)

	// --- Run the test binary inside the VM ---
	result := runCLISuccess(t, bin, "--instance", inst, "--ephemeral",
		"sh", "-c", fmt.Sprintf("FUSETEST_DIR=%s %s -test.v -test.timeout 60s",
			shareDir, filepath.Join(shareDir, "fusetest.test")),
	)

	// Verify key tests passed.
	assert.Contains(t, result, "PASS")
	assert.NotContains(t, result, "FAIL")
	t.Log(result)
}
