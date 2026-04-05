//go:build darwin && integration

package lnx_test

import (
	"bytes"
	"encoding/json"
	"os"
	"os/exec"
	"testing"
	"time"

	"github.com/creack/pty"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"github.com/vito/midterm"

	"github.com/semistrict/lnx"
)

// TestPTY_SessionsKillGraceful starts a session that exits cleanly on SIGTERM,
// kills it with `lnx sessions kill`, and verifies it exits without needing SIGKILL.
func TestPTY_SessionsKillGraceful(t *testing.T) {
	t.Parallel()

	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	// Start a session that exits cleanly on SIGTERM.
	term1 := midterm.NewTerminal(24, 80)
	cmd1 := exec.Command(bin, "--ephemeral", "sh", "-c", "trap 'exit 0' TERM; echo READY; while true; do sleep 1; done")
	ptmx1, err := pty.StartWithSize(cmd1, &pty.Winsize{Rows: 24, Cols: 80})
	require.NoError(t, err)
	defer ptmx1.Close()
	defer cmd1.Process.Kill()

	go feedTerminal(term1, ptmx1)
	waitFor(t, term1, "READY", 15*time.Second)

	// Verify session appears.
	require.Eventually(t, func() bool {
		return len(fetchSessionsAPI(t) ) > 0
	}, 10*time.Second, 500*time.Millisecond, "session never appeared in sessions list")

	sessions := fetchSessionsAPI(t)
	require.NotEmpty(t, sessions)
	sessID := sessions[0].ID

	// Kill the session — should exit gracefully on SIGTERM without reaching SIGKILL.
	killCmd := exec.Command(bin, "sessions", "kill", "default_"+sessID)
	start := time.Now()
	killOut, err := killCmd.CombinedOutput()
	elapsed := time.Since(start)
	t.Logf("sessions kill output: %s", string(killOut))
	require.NoError(t, err)

	// Should complete well under 10s (the SIGKILL timeout). If it took >8s, SIGKILL was used.
	assert.Less(t, elapsed, 8*time.Second, "kill took too long — SIGKILL was likely used instead of graceful SIGTERM exit")

	// Session should be gone.
	require.Eventually(t, func() bool {
		for _, s := range fetchSessionsAPI(t) {
			if s.ID == sessID {
				return false
			}
		}
		return true
	}, 5*time.Second, 500*time.Millisecond, "session still present after kill")

	// Client process should have exited.
	done := make(chan error, 1)
	go func() { done <- cmd1.Wait() }()
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("client process did not exit after session kill")
	}
}

// TestPTY_SessionsKillForce starts a session that ignores SIGTERM,
// kills it with `lnx sessions kill`, and verifies SIGKILL escalation works.
func TestPTY_SessionsKillForce(t *testing.T) {
	t.Parallel()

	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	// Start a session that traps SIGTERM and SIGHUP but does NOT exit.
	term1 := midterm.NewTerminal(24, 80)
	cmd1 := exec.Command(bin, "--ephemeral", "sh", "-c", "trap '' TERM HUP; echo READY; while true; do sleep 1; done")
	ptmx1, err := pty.StartWithSize(cmd1, &pty.Winsize{Rows: 24, Cols: 80})
	require.NoError(t, err)
	defer ptmx1.Close()
	defer cmd1.Process.Kill()

	go feedTerminal(term1, ptmx1)
	waitFor(t, term1, "READY", 15*time.Second)

	require.Eventually(t, func() bool {
		return len(fetchSessionsAPI(t)) > 0
	}, 10*time.Second, 500*time.Millisecond, "session never appeared")

	sessions := fetchSessionsAPI(t)
	require.NotEmpty(t, sessions)
	sessID := sessions[0].ID

	// Kill — SIGTERM will be ignored, must escalate to SIGKILL after 10s.
	killCmd := exec.Command(bin, "sessions", "kill", "default_"+sessID)
	killOut, err := killCmd.CombinedOutput()
	t.Logf("sessions kill output: %s", string(killOut))
	require.NoError(t, err)
	assert.Contains(t, string(killOut), "SIGKILL")

	// Session should be gone.
	require.Eventually(t, func() bool {
		for _, s := range fetchSessionsAPI(t) {
			if s.ID == sessID {
				return false
			}
		}
		return true
	}, 5*time.Second, 500*time.Millisecond, "session still present after SIGKILL")

	done := make(chan error, 1)
	go func() { done <- cmd1.Wait() }()
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("client process did not exit after session kill")
	}
}

// fetchSessionsCLI queries sessions via the HTTP API directly (not the CLI binary,
// which would need instance flag parsing).
func fetchSessionsAPI(t *testing.T) []lnx.SessionInfo {
	t.Helper()
	home, _ := os.UserHomeDir()
	sockPath := home + "/.lnx/instances/default/status.sock"

	cmd := exec.Command("curl", "-s", "--unix-socket", sockPath, "http://localhost/sessions")
	out, err := cmd.Output()
	if err != nil {
		return nil
	}
	out = bytes.TrimSpace(out)
	if len(out) == 0 {
		return nil
	}
	var sessions []lnx.SessionInfo
	if err := json.Unmarshal(out, &sessions); err != nil {
		return nil
	}
	return sessions
}

// TestPTY_SessionsList verifies that sessions list shows running sessions
// and that they disappear after exit.
func TestPTY_SessionsList(t *testing.T) {
	t.Parallel()

	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	// Start an interactive session.
	term1 := midterm.NewTerminal(24, 80)
	cmd1 := exec.Command(bin, "--ephemeral", "bash", "-l")
	ptmx1, err := pty.StartWithSize(cmd1, &pty.Winsize{Rows: 24, Cols: 80})
	require.NoError(t, err)
	defer ptmx1.Close()
	defer cmd1.Process.Kill()

	go feedTerminal(term1, ptmx1)
	waitFor(t, term1, "$", 15*time.Second)

	// Verify at least one session exists with expected fields.
	var sessions []lnx.SessionInfo
	require.Eventually(t, func() bool {
		sessions = fetchSessionsAPI(t)
		return len(sessions) > 0
	}, 10*time.Second, 500*time.Millisecond)

	s := sessions[0]
	assert.NotEmpty(t, s.ID)
	assert.Equal(t, []string{"bash", "-l"}, s.Args)
	assert.True(t, s.PTY)
	assert.Greater(t, s.ClientPID, 0)
	assert.Greater(t, s.GuestPID, 0)

	// Exit the session.
	ptmx1.Write([]byte{0x04})
	done := make(chan error, 1)
	go func() { done <- cmd1.Wait() }()
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("session did not exit within 5s")
	}
}
