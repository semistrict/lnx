//go:build darwin && integration

package lnx_test

import (
	"bytes"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"

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

func TestCLI_FollowupExecsAgainstRunningVM(t *testing.T) {
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	inst := "test-followup-exec"
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	cmd, stderr, done := startTimedInstance(t, bin, inst, 8*time.Second)
	t.Cleanup(func() { cleanupStreamingCLI(t, cmd, done, stderr) })

	for i := 0; i < 5; i++ {
		out := runCLISuccess(t, bin, "--instance", inst, "cat", "/etc/os-release")
		assert.Contains(t, out, `PRETTY_NAME="Ubuntu Resolute Raccoon (development branch)"`)
	}

	waitForProcessSuccess(t, done, 12*time.Second, stderr.String())
}

func TestCLI_StopWaitsUntilVMStops(t *testing.T) {
	bin, err := filepath.Abs("lnx")
	require.NoError(t, err)
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("skipping: repo lnx binary not found at %s", bin)
	}

	inst := "test-stop-waits"
	createClonedInstance(t, inst)
	registerInstanceStopCleanup(t, bin, inst)

	runCLISuccess(t, bin, "--instance", inst, "true")

	stopOut := runCLISuccess(t, bin, "--instance", inst, "stop")
	assert.Contains(t, stopOut, "VM stopped")

	statusOut, err := runCLI(bin, "--instance", inst, "status")
	require.NoError(t, err)
	assert.Contains(t, statusOut, "no VM running")
}

func TestCLI_EnvNotForwardedByDefault(t *testing.T) {
	bin, err := filepath.Abs("lnx")
	require.NoError(t, err)
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("skipping: repo lnx binary not found at %s", bin)
	}

	env := append(os.Environ(), "LNX_TEST_ENV=secret")
	out, err := runCLIEnv(bin, env, "--ephemeral", "sh", "-lc", `test -z "${LNX_TEST_ENV:-}" && echo OK`)
	require.NoError(t, err, out)
	assert.Contains(t, out, "OK")
}

func TestCLI_EnvForwardSpecificVar(t *testing.T) {
	bin, err := filepath.Abs("lnx")
	require.NoError(t, err)
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("skipping: repo lnx binary not found at %s", bin)
	}

	env := append(os.Environ(), "LNX_TEST_ENV=secret")
	out, err := runCLIEnv(bin, env, "--ephemeral", "--env", "LNX_TEST_ENV", "sh", "-lc", `test "$LNX_TEST_ENV" = secret && echo OK`)
	require.NoError(t, err, out)
	assert.Contains(t, out, "OK")
}

func TestCLI_EnvForwardAll(t *testing.T) {
	bin, err := filepath.Abs("lnx")
	require.NoError(t, err)
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("skipping: repo lnx binary not found at %s", bin)
	}

	env := append(os.Environ(), "LNX_TEST_ENV=secret", "LNX_TEST_ENV2=second")
	out, err := runCLIEnv(bin, env, "--ephemeral", "--env-all", "sh", "-lc", `test "$LNX_TEST_ENV" = secret && test "$LNX_TEST_ENV2" = second && echo OK`)
	require.NoError(t, err, out)
	assert.Contains(t, out, "OK")
}

func TestCLI_EnvForwardFromFile(t *testing.T) {
	bin, err := filepath.Abs("lnx")
	require.NoError(t, err)
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("skipping: repo lnx binary not found at %s", bin)
	}

	envFile := filepath.Join(t.TempDir(), ".env")
	require.NoError(t, os.WriteFile(envFile, []byte("LNX_FILE_ENV=secret\nLNX_FILE_ENV2=\"quoted value\"\n"), 0644))

	out, err := runCLIEnv(bin, os.Environ(), "--ephemeral", "--env", "@"+envFile, "sh", "-lc", `test "$LNX_FILE_ENV" = secret && test "$LNX_FILE_ENV2" = "quoted value" && echo OK`)
	require.NoError(t, err, out)
	assert.Contains(t, out, "OK")
}
