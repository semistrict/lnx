package lnx

import (
	"fmt"
	"os"
	"path/filepath"
	"time"

	"golang.org/x/sys/unix"
)

// checkpoint clones the rootfs to the checkpoint directory.
// Returns the path of the checkpoint.
func checkpoint(rootfsPath, checkpointDir string) (string, error) {
	if err := os.MkdirAll(checkpointDir, 0755); err != nil {
		return "", fmt.Errorf("create checkpoint dir: %w", err)
	}

	name := time.Now().Format("2006-01-02T15-04-05")
	dst := filepath.Join(checkpointDir, name+".ext4")

	if err := unix.Clonefile(rootfsPath, dst, 0); err != nil {
		return "", fmt.Errorf("clonefile %s -> %s: %w", rootfsPath, dst, err)
	}

	return dst, nil
}
