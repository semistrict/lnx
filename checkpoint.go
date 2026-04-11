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

// CreateCRIUCheckpoint clones both rootfs and CRIU volume into a
// checkpoint directory. The directory structure is:
//
//	checkpoints/<name>/
//	  rootfs.ext4   — APFS clone of rootfs
//	  criu.ext4     — APFS clone of CRIU images volume
//
// Returns the checkpoint directory path.
func CreateCRIUCheckpoint(rootfsPath, criuPath, checkpointDir string) (string, error) {
	if _, err := os.Stat(checkpointDir); err == nil {
		return "", fmt.Errorf("checkpoint %q already exists", filepath.Base(checkpointDir))
	}
	if err := os.MkdirAll(checkpointDir, 0755); err != nil {
		return "", fmt.Errorf("create checkpoint dir: %w", err)
	}

	rootfsDst := filepath.Join(checkpointDir, "rootfs.ext4")
	if err := cloneFile(rootfsPath, rootfsDst); err != nil {
		os.RemoveAll(checkpointDir)
		return "", fmt.Errorf("clone rootfs: %w", err)
	}

	criuDst := filepath.Join(checkpointDir, "criu.ext4")
	if err := cloneFile(criuPath, criuDst); err != nil {
		os.RemoveAll(checkpointDir)
		return "", fmt.Errorf("clone criu volume: %w", err)
	}

	return checkpointDir, nil
}
