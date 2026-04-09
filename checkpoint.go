package lnx

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"
)

// checkpoint clones the rootfs to the checkpoint directory.
// Returns the path of the checkpoint.
func checkpoint(rootfsPath, checkpointDir string) (string, error) {
	return CreateCheckpoint(rootfsPath, checkpointDir, "")
}

// CreateCheckpoint clones the rootfs to the checkpoint directory.
// If name is empty, a timestamp-based name is generated.
// Returns the path of the checkpoint.
func CreateCheckpoint(rootfsPath, checkpointDir, name string) (string, error) {
	if err := os.MkdirAll(checkpointDir, 0755); err != nil {
		return "", fmt.Errorf("create checkpoint dir: %w", err)
	}

	if name == "" {
		name = time.Now().Format("2006-01-02T15-04-05")
	}
	if filepath.Base(name) != name || name == "." || name == ".." {
		return "", fmt.Errorf("invalid checkpoint name %q", name)
	}
	if !strings.HasSuffix(name, ".ext4") {
		name += ".ext4"
	}
	dst := filepath.Join(checkpointDir, name)
	if _, err := os.Stat(dst); err == nil {
		return "", fmt.Errorf("checkpoint %q already exists", name)
	} else if !os.IsNotExist(err) {
		return "", fmt.Errorf("stat checkpoint %s: %w", dst, err)
	}

	if err := cloneFile(rootfsPath, dst); err != nil {
		return "", fmt.Errorf("clone %s -> %s: %w", rootfsPath, dst, err)
	}

	return dst, nil
}
