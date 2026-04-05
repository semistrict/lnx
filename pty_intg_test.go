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

func TestPTY_ExecInteractive(t *testing.T) {
	t.Parallel()

	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	// Start a VM in the background.
	vm := exec.Command(bin, "sleep", "120")
	require.NoError(t, vm.Start())
	defer vm.Process.Kill()

	// Wait for VM to be ready.
	time.Sleep(8 * time.Second)

	// Exec into it with a PTY.
	term := midterm.NewTerminal(24, 80)

	cmd := exec.Command(bin, "exec", "bash")
	ptmx, err := pty.StartWithSize(cmd, &pty.Winsize{Rows: 24, Cols: 80})
	require.NoError(t, err)
	defer ptmx.Close()
	defer cmd.Process.Kill()

	go feedTerminal(term, ptmx)

	waitFor(t, term, "$", 15*time.Second)

	ptmx.WriteString("echo EXEC_WORKS\n")
	waitFor(t, term, "EXEC_WORKS", 5*time.Second)

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
