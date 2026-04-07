//go:build linux

package main

import (
	"os"
	"path/filepath"
)

func resolveKernel() string {
	// Firecracker needs a different kernel than VZ.
	// Check for the Firecracker-specific kernel first.
	base := lnxBase()
	fcKernel := filepath.Join(base, "vmlinuz-firecracker")
	if _, err := os.Stat(fcKernel); err == nil {
		return fcKernel
	}
	// Fall back to the generic kernel.
	return filepath.Join(base, "vmlinuz")
}
