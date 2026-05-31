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
	if findDefaultRootfs(base) == "" {
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

func TestCLI_PreserveEnv(t *testing.T) {
	bin, err := filepath.Abs("lnx")
	require.NoError(t, err)
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("skipping: repo lnx binary not found at %s", bin)
	}

	hostHome := os.Getenv("HOME")
	hostPath := os.Getenv("PATH")
	require.NotEmpty(t, hostHome)
	require.NotEmpty(t, hostPath)

	env := append(os.Environ(),
		"LNX_TEST_ENV=secret",
		"LNX_TEST_ENV2=second",
	)
	script := `test "$LNX_TEST_ENV" = secret && test "$LNX_TEST_ENV2" = second && test "$HOME" != "` + hostHome + `" && test "$PATH" != "` + hostPath + `" && echo OK`
	out, err := runCLIEnv(bin, env, "--ephemeral", "--preserve-env", "sh", "-lc", script)
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

func TestCLI_CheckpointsCreateStoppedAndRunning(t *testing.T) {
	bin, err := filepath.Abs("lnx")
	require.NoError(t, err)
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("skipping: repo lnx binary not found at %s", bin)
	}

	home, _ := os.UserHomeDir()
	base := filepath.Join(home, ".lnx")

	stoppedInst := "test-checkpoint-stopped"
	createClonedInstance(t, stoppedInst)
	stoppedOut := runCLISuccess(t, bin, "--instance", stoppedInst, "checkpoints", "create", "stopped")
	assert.Contains(t, stoppedOut, `created checkpoint "stopped.ext4"`)
	_, err = os.Stat(filepath.Join(base, "images", stoppedInst, "checkpoints", "stopped.ext4"))
	require.NoError(t, err)

	runningInst := "test-checkpoint-running"
	createClonedInstance(t, runningInst)
	registerInstanceStopCleanup(t, bin, runningInst)

	cmd, stderr, done := startTimedInstance(t, bin, runningInst, 8*time.Second)
	t.Cleanup(func() { cleanupStreamingCLI(t, cmd, done, stderr) })

	runningOut := runCLISuccess(t, bin, "--instance", runningInst, "checkpoints", "create", "running")
	assert.Contains(t, runningOut, `created checkpoint "running.ext4"`)
	_, err = os.Stat(filepath.Join(base, "images", runningInst, "checkpoints", "running.ext4"))
	require.NoError(t, err)

	waitForProcessSuccess(t, done, 12*time.Second, stderr.String())
}

func TestCLI_InstanceCreateFromNamedCheckpointCopiesMetadata(t *testing.T) {
	bin, err := filepath.Abs("lnx")
	require.NoError(t, err)
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("skipping: repo lnx binary not found at %s", bin)
	}

	home, _ := os.UserHomeDir()
	base := filepath.Join(home, ".lnx")

	srcInst := "test-clone-from-checkpoint-src"
	dstInst := "test-clone-from-checkpoint-dst"
	createClonedInstance(t, srcInst)
	registerInstanceStopCleanup(t, bin, srcInst, dstInst)
	t.Cleanup(func() {
		_ = os.RemoveAll(filepath.Join(base, "instances", dstInst))
		_ = os.RemoveAll(filepath.Join(base, "images", dstInst))
	})

	shareDir := t.TempDir()
	runCLISuccess(t, bin, "--instance", srcInst, "share", "add", shareDir)
	runCLISuccess(t, bin, "--instance", srcInst, "sh", "-lc", `echo from-checkpoint > "$HOME/from-checkpoint.txt"`)
	runCLISuccess(t, bin, "--instance", srcInst, "checkpoints", "create", "base")
	runCLISuccess(t, bin, "--instance", srcInst, "sh", "-lc", `echo after-checkpoint > "$HOME/after-checkpoint.txt"`)

	out := runCLISuccess(t, bin, "--instance", srcInst, "clone", "--checkpoint", "base", dstInst)
	assert.Contains(t, out, `created instance "`+dstInst+`" from "`+srcInst+`:base"`)

	fromCheckpoint := runCLISuccess(t, bin, "--instance", dstInst, "sh", "-lc", `cat "$HOME/from-checkpoint.txt"`)
	assert.Equal(t, "from-checkpoint\n", fromCheckpoint)

	missingOut, err := runCLI(bin, "--instance", dstInst, "sh", "-lc", `cat "$HOME/after-checkpoint.txt"`)
	require.Error(t, err)
	assert.Contains(t, missingOut, "No such file")

	shareOut := runCLISuccess(t, bin, "--instance", dstInst, "share", "list")
	assert.Contains(t, shareOut, shareDir)

	// Checkpoint should not be copied to destination instance images dir.
	_, err = os.Stat(filepath.Join(base, "images", dstInst, "checkpoints", "base.ext4"))
	require.ErrorIs(t, err, os.ErrNotExist)
}

func TestCLI_InstanceCreateFromRunningSourceAutoCheckpoint(t *testing.T) {
	bin, err := filepath.Abs("lnx")
	require.NoError(t, err)
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("skipping: repo lnx binary not found at %s", bin)
	}

	home, _ := os.UserHomeDir()
	base := filepath.Join(home, ".lnx")

	srcInst := "test-clone-running-src"
	dstInst := "test-clone-running-dst"
	createClonedInstance(t, srcInst)
	registerInstanceStopCleanup(t, bin, srcInst, dstInst)
	t.Cleanup(func() {
		_ = os.RemoveAll(filepath.Join(base, "instances", dstInst))
		_ = os.RemoveAll(filepath.Join(base, "images", dstInst))
	})

	shareDir := t.TempDir()
	runCLISuccess(t, bin, "--instance", srcInst, "share", "add", shareDir)

	cmd, stderr, done := startTimedInstance(t, bin, srcInst, 8*time.Second)
	t.Cleanup(func() { cleanupStreamingCLI(t, cmd, done, stderr) })

	runCLISuccess(t, bin, "--instance", srcInst, "sh", "-lc", `echo running-checkpoint > "$HOME/running-checkpoint.txt"`)

	out := runCLISuccess(t, bin, "--instance", srcInst, "clone", dstInst)
	assert.Contains(t, out, `created instance "`+dstInst+`" from "`+srcInst+`"`)

	cloned := runCLISuccess(t, bin, "--instance", dstInst, "sh", "-lc", `cat "$HOME/running-checkpoint.txt"`)
	assert.Equal(t, "running-checkpoint\n", cloned)

	shareOut := runCLISuccess(t, bin, "--instance", dstInst, "share", "list")
	assert.Contains(t, shareOut, shareDir)

	checkpoints, err := filepath.Glob(filepath.Join(base, "images", srcInst, "checkpoints", "*.ext4"))
	require.NoError(t, err)
	assert.NotEmpty(t, checkpoints)

	_, err = os.Stat(filepath.Join(base, "images", dstInst, "checkpoints"))
	require.ErrorIs(t, err, os.ErrNotExist)

	waitForProcessSuccess(t, done, 12*time.Second, stderr.String())
}
