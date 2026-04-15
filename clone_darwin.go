//go:build darwin

package lnx

import "golang.org/x/sys/unix"

// cloneFile creates a copy-on-write clone of src at dst using APFS clonefile.
func cloneFile(src, dst string) error {
	return unix.Clonefile(src, dst, 0)
}

// CloneFile is the exported version of cloneFile for tests.
func CloneFile(src, dst string) error {
	return cloneFile(src, dst)
}
