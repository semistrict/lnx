// Command codesign is a test executor that signs the test binary with the
// virtualization entitlement before running it. Use with:
//
//	go test -exec "go run ./cmd/codesign" -tags integration ./...
package main

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
)

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "usage: codesign <test-binary> [args...]")
		os.Exit(1)
	}

	binary := os.Args[1]

	// Find entitlements.plist relative to this file's module root.
	entitlements := findEntitlements()

	sign := exec.Command("codesign", "--entitlements", entitlements, "--force", "-s", "-", binary)
	sign.Stderr = os.Stderr
	if err := sign.Run(); err != nil {
		fmt.Fprintf(os.Stderr, "codesign failed: %v\n", err)
		os.Exit(1)
	}

	cmd := exec.Command(binary, os.Args[2:]...)
	cmd.Stdin = os.Stdin
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr

	if err := cmd.Run(); err != nil {
		if exitErr, ok := err.(*exec.ExitError); ok {
			os.Exit(exitErr.ExitCode())
		}
		fmt.Fprintf(os.Stderr, "exec failed: %v\n", err)
		os.Exit(1)
	}
}

func findEntitlements() string {
	// Walk up from cwd to find entitlements.plist.
	dir, _ := os.Getwd()
	for {
		p := filepath.Join(dir, "entitlements.plist")
		if _, err := os.Stat(p); err == nil {
			return p
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			break
		}
		dir = parent
	}
	// Fallback.
	return "entitlements.plist"
}
