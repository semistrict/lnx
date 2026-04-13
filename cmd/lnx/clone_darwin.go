//go:build darwin

package main

import (
	"errors"
	"io"
	"os"
	"syscall"

	"golang.org/x/sys/unix"
)

// cloneRootfs creates a copy-on-write clone of src at dst using APFS clonefile.
// Falls back to a regular copy if the source and destination are on different volumes.
func cloneRootfs(src, dst string) error {
	err := unix.Clonefile(src, dst, 0)
	if err == nil || !errors.Is(err, syscall.EXDEV) {
		return err
	}
	return copyRootfs(src, dst)
}

func copyRootfs(src, dst string) error {
	s, err := os.Open(src)
	if err != nil {
		return err
	}
	defer s.Close()
	info, err := s.Stat()
	if err != nil {
		return err
	}
	d, err := os.OpenFile(dst, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, info.Mode())
	if err != nil {
		return err
	}
	if _, err := io.Copy(d, s); err != nil {
		d.Close()
		os.Remove(dst)
		return err
	}
	return d.Close()
}
