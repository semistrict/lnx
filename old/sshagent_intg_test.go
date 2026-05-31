//go:build darwin && integration

package lnx_test

import (
	"os"
	"os/exec"
	"strings"
	"testing"
	"time"

	"github.com/creack/pty"
	"github.com/stretchr/testify/require"
	"github.com/vito/midterm"
)

// TestPTY_SSHAgentForward verifies that --ssh-agent forwards the host's
// SSH agent into the guest. The guest should see SSH_AUTH_SOCK set and
// ssh-add -l should not error with "could not open a connection".
func TestPTY_SSHAgentForward(t *testing.T) {
	t.Parallel()

	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	if os.Getenv("SSH_AUTH_SOCK") == "" {
		t.Skip("no SSH_AUTH_SOCK on host")
	}

	term := midterm.NewTerminal(24, 80)

	cmd := exec.Command(bin, "--ephemeral", "--ssh-agent", "sh", "-c",
		`echo "SOCK=$SSH_AUTH_SOCK"; test -S "$SSH_AUTH_SOCK" && echo SOCKET_EXISTS || echo SOCKET_MISSING`)
	ptmx, err := pty.StartWithSize(cmd, &pty.Winsize{Rows: 24, Cols: 80})
	require.NoError(t, err)
	defer ptmx.Close()
	defer cmd.Process.Kill()

	go feedTerminal(term, ptmx)

	// Wait for the test output.
	waitFor(t, term, "SOCK=", 15*time.Second)

	screen := screenText(term)

	// SSH_AUTH_SOCK should be set.
	if !strings.Contains(screen, "SOCK=/tmp/ssh-agent.sock") {
		t.Fatalf("SSH_AUTH_SOCK not set in guest.\nScreen:\n%s", screen)
	}

	// The socket file should exist.
	if !strings.Contains(screen, "SOCKET_EXISTS") {
		t.Fatalf("SSH agent socket does not exist in guest.\nScreen:\n%s", screen)
	}
}
