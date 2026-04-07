//go:build linux

package lnx

import (
	"fmt"
	"io"
	"os"
)

// cloneFile copies src to dst. On Linux there's no APFS clonefile;
// the kernel may use reflinks transparently if the filesystem supports it.
func cloneFile(src, dst string) error {
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
