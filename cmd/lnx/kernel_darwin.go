//go:build darwin

package main

import "path/filepath"

func resolveKernel() string {
	return filepath.Join(lnxBase(), "vmlinuz")
}
