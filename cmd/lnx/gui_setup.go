package main

import (
	"bufio"
	"fmt"
	"os"
	"os/exec"
	"strings"

	"github.com/semistrict/lnx"
)

const brewInstallCmd = "brew tap J-x-Z/tap && brew install cocoa-way waypipe-darwin"

// ensureGUIBinaries checks that cocoa-way and waypipe-darwin are installed.
// If missing, prompts the user to install via Homebrew.
func ensureGUIBinaries() error {
	missing := lnx.MissingGUIDeps()
	if len(missing) == 0 {
		return nil
	}

	fmt.Fprintf(os.Stderr, "GUI mode requires %s, which are not installed.\n\n", strings.Join(missing, " and "))
	fmt.Fprintf(os.Stderr, "  %s\n\n", brewInstallCmd)
	fmt.Fprintf(os.Stderr, "Would you like to run this now? [Y/n] ")

	reader := bufio.NewReader(os.Stdin)
	answer, _ := reader.ReadString('\n')
	answer = strings.TrimSpace(strings.ToLower(answer))

	if answer != "" && answer != "y" && answer != "yes" {
		fmt.Fprintln(os.Stderr, "Skipping install. You can install manually and retry.")
		return nil
	}

	fmt.Fprintln(os.Stderr, "Installing...")
	cmd := exec.Command("bash", "-c", brewInstallCmd)
	cmd.Stdout = os.Stderr
	cmd.Stderr = os.Stderr
	cmd.Stdin = os.Stdin
	if err := cmd.Run(); err != nil {
		fmt.Fprintf(os.Stderr, "brew install failed: %v\nYou can try installing manually.\n", err)
	}

	// Verify after install attempt.
	missing = lnx.MissingGUIDeps()
	if len(missing) > 0 {
		fmt.Fprintf(os.Stderr, "Warning: %s still not found in PATH. GUI may not work.\n", strings.Join(missing, ", "))
	}
	return nil
}
