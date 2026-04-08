//go:build darwin && integration

package lnx_test

import (
	"bytes"
	"os"
	"os/exec"
	"path/filepath"
	"testing"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestCLI_MissingCommand(t *testing.T) {
	t.Parallel()

	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	home, _ := os.UserHomeDir()
	base := filepath.Join(home, ".lnx")
	if _, err := os.Stat(filepath.Join(base, "vmlinuz")); err != nil {
		t.Skipf("skipping: vmlinuz not found in ~/.lnx (run 'lnx init' first)")
	}
	if _, err := os.Stat(filepath.Join(base, "instances", "default", "rootfs.ext4")); err != nil {
		t.Skipf("skipping: default instance rootfs not found (run 'lnx init' first)")
	}

	cmd := exec.Command(bin, "--ephemeral", "doesnotexist42")
	var stdout bytes.Buffer
	var stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr

	err := cmd.Run()
	require.Error(t, err)

	exitErr, ok := err.(*exec.ExitError)
	require.True(t, ok, "expected process exit error, got %T: %v", err, err)
	assert.Equal(t, 127, exitErr.ExitCode())
	assert.Contains(t, stderr.String(), "doesnotexist42: command not found")
}
