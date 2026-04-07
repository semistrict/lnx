//go:build darwin

package main

import "os/exec"

// buildDaemonCmd creates the command to spawn the daemon process.
// On Darwin, no privilege escalation is needed.
func buildDaemonCmd(self string, daemonArgs []string) *exec.Cmd {
	return exec.Command(self, daemonArgs...)
}
