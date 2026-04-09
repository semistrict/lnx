package main

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/stretchr/testify/require"
)

func TestExecEnvExplicitForwarding(t *testing.T) {
	t.Setenv("FORWARD_ME", "value")
	forwardEnv = []string{"FORWARD_ME", "SET_ME=literal"}
	forwardAllEnv = false
	t.Cleanup(func() {
		forwardEnv = nil
		forwardAllEnv = false
	})

	env, err := execEnv()
	require.NoError(t, err)
	require.Equal(t, []string{"FORWARD_ME=value", "SET_ME=literal"}, env)
}

func TestExecEnvMissingVar(t *testing.T) {
	forwardEnv = []string{"DOES_NOT_EXIST_42"}
	forwardAllEnv = false
	t.Cleanup(func() {
		forwardEnv = nil
		forwardAllEnv = false
	})

	_, err := execEnv()
	require.Error(t, err)
	require.Contains(t, err.Error(), `host env var "DOES_NOT_EXIST_42" is not set`)
}

func TestExecEnvDotenvFile(t *testing.T) {
	path := filepath.Join(t.TempDir(), ".env")
	require.NoError(t, os.WriteFile(path, []byte("ZED=last\nALPHA=\"first value\"\n"), 0644))

	forwardEnv = []string{"@" + path}
	forwardAllEnv = false
	t.Cleanup(func() {
		forwardEnv = nil
		forwardAllEnv = false
	})

	env, err := execEnv()
	require.NoError(t, err)
	require.Equal(t, []string{"ALPHA=first value", "ZED=last"}, env)
}

func TestExecEnvPreserveEnvExcludesHostPathVars(t *testing.T) {
	t.Setenv("HOME", "/host/home")
	t.Setenv("PATH", "/host/bin")
	t.Setenv("PWD", "/host/pwd")
	t.Setenv("LNX_KEEP_ME", "yes")
	forwardEnv = nil
	forwardAllEnv = true
	t.Cleanup(func() {
		forwardEnv = nil
		forwardAllEnv = false
	})

	env, err := execEnv()
	require.NoError(t, err)
	require.Contains(t, env, "LNX_KEEP_ME=yes")
	require.NotContains(t, env, "HOME=/host/home")
	require.NotContains(t, env, "PATH=/host/bin")
	require.NotContains(t, env, "PWD=/host/pwd")
}
