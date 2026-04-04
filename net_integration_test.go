//go:build darwin && integration

package lnx_test

import (
	"testing"

	"github.com/semistrict/lnx"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestRun_NetworkPing(t *testing.T) {
	dir := setupTestDir(t)
	cfg := testConfig(dir)

	exitCode, err := lnx.Run(cfg, "ping", "-c1", "-W3", "8.8.8.8")
	require.NoError(t, err)
	assert.Equal(t, 0, exitCode)
}

func TestRun_NetworkHTTP(t *testing.T) {
	dir := setupTestDir(t)
	cfg := testConfig(dir)

	exitCode, err := lnx.Run(cfg, "curl", "-s", "--max-time", "5", "-o", "/dev/null", "-w", "%{http_code}", "http://example.com")
	require.NoError(t, err)
	assert.Equal(t, 0, exitCode)
}
