package lnx

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"
)

// CheckpointType identifies how a checkpoint was created.
type CheckpointType string

const (
	CheckpointTypeDisk   CheckpointType = "disk"
	CheckpointTypeMemory CheckpointType = "memory"
)

// CheckpointMetadata describes a checkpoint's metadata.
type CheckpointMetadata struct {
	Name        string         `json:"name"`
	Type        CheckpointType `json:"type"`
	Description string         `json:"description,omitempty"`
	Tags        []string       `json:"tags,omitempty"`
	CreatedAt   time.Time      `json:"created_at"`
}

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

// CreateMemoryCheckpoint clones both rootfs and swap into a named checkpoint
// directory and writes metadata.json. The caller must ensure the VM is
// hibernated (rootfs and swap are quiescent) before calling this.
func CreateMemoryCheckpoint(rootfsPath, swapPath, checkpointDir, name, description string, tags []string) (*CheckpointMetadata, error) {
	if err := os.MkdirAll(checkpointDir, 0755); err != nil {
		return nil, fmt.Errorf("create checkpoint dir: %w", err)
	}

	if name == "" {
		name = time.Now().Format("2006-01-02T15-04-05")
	}
	if filepath.Base(name) != name || name == "." || name == ".." {
		return nil, fmt.Errorf("invalid checkpoint name %q", name)
	}

	cpDir := filepath.Join(checkpointDir, name)
	if _, err := os.Stat(cpDir); err == nil {
		return nil, fmt.Errorf("checkpoint %q already exists", name)
	} else if !os.IsNotExist(err) {
		return nil, fmt.Errorf("stat checkpoint %s: %w", cpDir, err)
	}

	if err := os.MkdirAll(cpDir, 0755); err != nil {
		return nil, fmt.Errorf("create checkpoint subdir: %w", err)
	}

	if err := cloneFile(rootfsPath, filepath.Join(cpDir, "rootfs.ext4")); err != nil {
		os.RemoveAll(cpDir)
		return nil, fmt.Errorf("clone rootfs: %w", err)
	}

	if err := cloneFile(swapPath, filepath.Join(cpDir, "swap.img")); err != nil {
		os.RemoveAll(cpDir)
		return nil, fmt.Errorf("clone swap: %w", err)
	}

	meta := &CheckpointMetadata{
		Name:        name,
		Type:        CheckpointTypeMemory,
		Description: description,
		Tags:        tags,
		CreatedAt:   time.Now(),
	}

	metaBytes, err := json.MarshalIndent(meta, "", "  ")
	if err != nil {
		os.RemoveAll(cpDir)
		return nil, fmt.Errorf("marshal metadata: %w", err)
	}
	if err := os.WriteFile(filepath.Join(cpDir, "metadata.json"), metaBytes, 0644); err != nil {
		os.RemoveAll(cpDir)
		return nil, fmt.Errorf("write metadata: %w", err)
	}

	return meta, nil
}

// RestoreMemoryCheckpoint replaces the live rootfs and swap with clones from
// a memory checkpoint directory. The caller should shut down the VM first.
func RestoreMemoryCheckpoint(checkpointDir, name, rootfsPath, swapPath string) error {
	cpDir := filepath.Join(checkpointDir, name)

	cpRootfs := filepath.Join(cpDir, "rootfs.ext4")
	cpSwap := filepath.Join(cpDir, "swap.img")

	if _, err := os.Stat(cpRootfs); err != nil {
		return fmt.Errorf("checkpoint rootfs not found: %w", err)
	}
	if _, err := os.Stat(cpSwap); err != nil {
		return fmt.Errorf("checkpoint swap not found: %w", err)
	}

	// Remove the live files and replace with clones from the checkpoint.
	if err := os.Remove(rootfsPath); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("remove live rootfs: %w", err)
	}
	if err := cloneFile(cpRootfs, rootfsPath); err != nil {
		return fmt.Errorf("clone checkpoint rootfs: %w", err)
	}

	if err := os.Remove(swapPath); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("remove live swap: %w", err)
	}
	if err := cloneFile(cpSwap, swapPath); err != nil {
		return fmt.Errorf("clone checkpoint swap: %w", err)
	}

	return nil
}

// ListCheckpoints scans the checkpoint directory for both legacy disk-only
// checkpoints (.ext4 files) and memory checkpoints (directories with
// metadata.json).
func ListCheckpoints(checkpointDir string) ([]CheckpointMetadata, error) {
	entries, err := os.ReadDir(checkpointDir)
	if err != nil {
		if os.IsNotExist(err) {
			return nil, nil
		}
		return nil, err
	}

	var result []CheckpointMetadata
	for _, e := range entries {
		if e.IsDir() {
			// Memory checkpoint directory — read metadata.json.
			metaPath := filepath.Join(checkpointDir, e.Name(), "metadata.json")
			data, err := os.ReadFile(metaPath)
			if err != nil {
				continue // not a checkpoint directory
			}
			var meta CheckpointMetadata
			if err := json.Unmarshal(data, &meta); err != nil {
				continue
			}
			result = append(result, meta)
		} else if filepath.Ext(e.Name()) == ".ext4" {
			// Legacy disk-only checkpoint.
			info, err := e.Info()
			if err != nil {
				continue
			}
			result = append(result, CheckpointMetadata{
				Name:      e.Name(),
				Type:      CheckpointTypeDisk,
				CreatedAt: info.ModTime(),
			})
		}
	}

	return result, nil
}

// DeleteCheckpoint removes a checkpoint — either a legacy .ext4 file or a
// memory checkpoint directory.
func DeleteCheckpoint(checkpointDir, name string) error {
	// Try memory checkpoint directory first.
	dirPath := filepath.Join(checkpointDir, name)
	if info, err := os.Stat(dirPath); err == nil && info.IsDir() {
		return os.RemoveAll(dirPath)
	}

	// Try legacy .ext4 file.
	ext4Name := name
	if !strings.HasSuffix(ext4Name, ".ext4") {
		ext4Name += ".ext4"
	}
	filePath := filepath.Join(checkpointDir, ext4Name)
	if _, err := os.Stat(filePath); err == nil {
		return os.Remove(filePath)
	}

	return fmt.Errorf("checkpoint %q not found", name)
}
