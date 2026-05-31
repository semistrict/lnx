//go:build darwin && integration

package lnx_test

import (
	"os"
	"os/exec"
	"path/filepath"
	"testing"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestCLI_SharedMemoryMountExists(t *testing.T) {
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	home, _ := os.UserHomeDir()
	base := filepath.Join(home, ".lnx")
	if _, err := os.Stat(filepath.Join(base, "vmlinuz")); err != nil {
		t.Skipf("skipping: vmlinuz not found in ~/.lnx (run 'lnx init' first)")
	}
	if findDefaultRootfs(base) == "" {
		t.Skipf("skipping: default instance rootfs not found (run 'lnx init' first)")
	}

	cmd := exec.Command(bin, "--ephemeral", "sh", "-lc", `set -e
test -d /dev/shm
mount | grep ' on /dev/shm type tmpfs '
`)
	out, err := cmd.CombinedOutput()
	require.NoError(t, err, "shared memory mount check failed: %s", out)
	assert.Contains(t, string(out), "on /dev/shm type tmpfs")
}
