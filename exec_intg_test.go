//go:build darwin && integration

package lnx_test

import (
	"bytes"
	"context"
	"encoding/json"
	"net"
	"net/http"
	"path/filepath"
	"testing"
	"time"

	"github.com/semistrict/lnx"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestRun_ExecIntoRunningVM(t *testing.T) {
	t.Parallel()
	dir := setupTestDir(t)
	cfg := testConfig(dir)

	// Boot VM with a long-running command.
	go lnx.Run(cfg, "sleep", "60")

	// Wait for the API socket and exec to be ready.
	sockPath := filepath.Join(dir, "status.sock")
	client := &http.Client{
		Transport: &http.Transport{
			DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
				return net.DialTimeout("unix", sockPath, 2*time.Second)
			},
		},
	}

	// Wait until exec endpoint is ready by probing it.
	require.Eventually(t, func() bool {
		body, _ := json.Marshal(lnx.ExecRequest{Args: []string{"true"}})
		resp, err := client.Post("http://localhost/exec", "application/json", bytes.NewReader(body))
		if err != nil {
			return false
		}
		resp.Body.Close()
		return resp.StatusCode == http.StatusOK
	}, 60*time.Second, time.Second, "VM exec never became ready")

	// Exec a non-interactive command into the running VM.
	body, err := json.Marshal(lnx.ExecRequest{Args: []string{"echo", "EXEC_WORKS"}})
	require.NoError(t, err)

	resp, err := client.Post("http://localhost/exec", "application/json", bytes.NewReader(body))
	require.NoError(t, err)
	defer resp.Body.Close()
	assert.Equal(t, http.StatusOK, resp.StatusCode)

	// Read NDJSON response.
	var output string
	var exitCode int = -1
	dec := json.NewDecoder(resp.Body)
	for {
		var msg map[string]json.RawMessage
		if err := dec.Decode(&msg); err != nil {
			break
		}
		if raw, ok := msg["stdout"]; ok {
			var s string
			json.Unmarshal(raw, &s)
			output += s
		}
		if raw, ok := msg["exit_code"]; ok {
			json.Unmarshal(raw, &exitCode)
		}
	}

	assert.Contains(t, output, "EXEC_WORKS")
	assert.Equal(t, 0, exitCode)
}
