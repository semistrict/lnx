//go:build darwin

package main

import "golang.org/x/sys/unix"

// cloneRootfs creates a copy-on-write clone of src at dst using APFS clonefile.
func cloneRootfs(src, dst string) error {
	return unix.Clonefile(src, dst, 0)
}
