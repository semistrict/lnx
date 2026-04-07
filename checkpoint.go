package lnx

import (
	"fmt"
	"os"
	"path/filepath"
	"time"
)

// checkpoint clones the rootfs to the checkpoint directory.
// Returns the path of the checkpoint.
func checkpoint(rootfsPath, checkpointDir string) (string, error) {
	if err := os.MkdirAll(checkpointDir, 0755); err != nil {
		return "", fmt.Errorf("create checkpoint dir: %w", err)
	}

	name := time.Now().Format("2006-01-02T15-04-05")
	dst := filepath.Join(checkpointDir, name+".ext4")

	if err := cloneFile(rootfsPath, dst); err != nil {
		return "", fmt.Errorf("clone %s -> %s: %w", rootfsPath, dst, err)
	}

	return dst, nil
}
