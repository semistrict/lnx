//go:build linux

package main

import (
	"fmt"
	"io"
	"os"
)

// cloneRootfs copies src to dst. On Linux, this is a regular file copy
// (the filesystem may use reflinks if supported, e.g. btrfs/xfs).
func cloneRootfs(src, dst string) error {
	sf, err := os.Open(src)
	if err != nil {
		return fmt.Errorf("open source: %w", err)
	}
	defer sf.Close()

	df, err := os.Create(dst)
	if err != nil {
		return fmt.Errorf("create dest: %w", err)
	}
	defer df.Close()

	if _, err := io.Copy(df, sf); err != nil {
		os.Remove(dst)
		return fmt.Errorf("copy: %w", err)
	}
	return df.Close()
}
