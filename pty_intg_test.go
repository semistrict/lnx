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

// screenText returns the visible text from a midterm terminal.
func screenText(term *midterm.Terminal) string {
	var lines []string
	for _, row := range term.Content {
		lines = append(lines, strings.TrimRight(string(row), " \x00"))
	}
	return strings.TrimRight(strings.Join(lines, "\n"), "\n")
}

// feedTerminal reads from ptmx and writes to the virtual terminal.
func feedTerminal(term *midterm.Terminal, ptmx *os.File) {
	buf := make([]byte, 4096)
	for {
		n, err := ptmx.Read(buf)
		if n > 0 {
			term.Write(buf[:n])
		}
		if err != nil {
			return
		}
	}
}

// waitFor polls the terminal screen until it contains the expected string.
func waitFor(t testing.TB, term *midterm.Terminal, want string, timeout time.Duration) {
	t.Helper()
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		if strings.Contains(screenText(term), want) {
			return
		}
		time.Sleep(100 * time.Millisecond)
	}
	t.Fatalf("timed out waiting for %q in terminal:\n%s", want, screenText(term))
}

func lnxBin() string {
	p, _ := exec.LookPath("lnx")
	return p
}

func TestPTY_MainInteractive(t *testing.T) {
	t.Parallel()

	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	term := midterm.NewTerminal(24, 80)

	cmd := exec.Command(bin, "--ephemeral", "bash", "-l")
	ptmx, err := pty.StartWithSize(cmd, &pty.Winsize{Rows: 24, Cols: 80})
	require.NoError(t, err)
	defer ptmx.Close()
	defer cmd.Process.Kill()

	go feedTerminal(term, ptmx)

	// Wait for a shell prompt ($ is common to bash prompts).
	waitFor(t, term, "$", 15*time.Second)

	ptmx.WriteString("echo HELLO_FROM_PTY\n")
	waitFor(t, term, "HELLO_FROM_PTY", 5*time.Second)

	// Ctrl-D should exit cleanly.
	ptmx.Write([]byte{0x04})

	done := make(chan error, 1)
	go func() { done <- cmd.Wait() }()
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("Ctrl-D did not exit within 5s")
	}
}

func TestPTY_SecondSession(t *testing.T) {
	t.Parallel()

	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	// Start a first interactive session — this auto-starts the daemon.
	term1 := midterm.NewTerminal(24, 80)
	cmd1 := exec.Command(bin, "--ephemeral", "bash", "-l")
	ptmx1, err := pty.StartWithSize(cmd1, &pty.Winsize{Rows: 24, Cols: 80})
	require.NoError(t, err)
	defer ptmx1.Close()
	defer cmd1.Process.Kill()

	go feedTerminal(term1, ptmx1)
	waitFor(t, term1, "$", 15*time.Second)

	// Start a second session that execs into the running VM.
	term2 := midterm.NewTerminal(24, 80)
	cmd2 := exec.Command(bin, "bash", "-l")
	ptmx2, err := pty.StartWithSize(cmd2, &pty.Winsize{Rows: 24, Cols: 80})
	require.NoError(t, err)
	defer ptmx2.Close()
	defer cmd2.Process.Kill()

	go feedTerminal(term2, ptmx2)
	waitFor(t, term2, "$", 15*time.Second)

	ptmx2.WriteString("echo SECOND_SESSION\n")
	waitFor(t, term2, "SECOND_SESSION", 5*time.Second)

	// Exit second session.
	ptmx2.Write([]byte{0x04})
	done2 := make(chan error, 1)
	go func() { done2 <- cmd2.Wait() }()
	select {
	case <-done2:
	case <-time.After(5 * time.Second):
		t.Fatal("second session did not exit within 5s")
	}

	// Exit first session — daemon should shut down after this.
	ptmx1.Write([]byte{0x04})
	done1 := make(chan error, 1)
	go func() { done1 <- cmd1.Wait() }()
	select {
	case <-done1:
	case <-time.After(5 * time.Second):
		t.Fatal("first session did not exit within 5s")
	}
}

func TestPTY_InteractiveCommandNotFoundShowsMessage(t *testing.T) {
	t.Parallel()

	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	term := midterm.NewTerminal(24, 100)

	cmd := exec.Command(bin, "omx")
	ptmx, err := pty.StartWithSize(cmd, &pty.Winsize{Rows: 24, Cols: 100})
	require.NoError(t, err)
	defer ptmx.Close()
	defer cmd.Process.Kill()

	go feedTerminal(term, ptmx)

	waitFor(t, term, "omx: command not found", 10*time.Second)

	done := make(chan error, 1)
	go func() { done <- cmd.Wait() }()
	select {
	case err := <-done:
		require.Error(t, err)
	case <-time.After(5 * time.Second):
		t.Fatal("interactive command-not-found did not exit within 5s")
	}
}
