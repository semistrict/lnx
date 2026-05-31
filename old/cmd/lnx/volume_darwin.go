//go:build darwin

package main

import (
	"encoding/xml"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
)

const volumeName = "lnx"

// volumeInfo holds parsed diskutil info for an APFS volume.
type volumeInfo struct {
	VolumeName       string
	MountPoint       string
	FilesystemType   string
	CapacityInUse    uint64 // physical bytes consumed by this volume
	ContainerSize    uint64 // total APFS container size
	ContainerFree    uint64 // free space in APFS container
	ContainerRef     string // e.g. "disk3"
	DeviceIdentifier string // e.g. "disk3s7"
}

// getVolumeInfo runs `diskutil info -plist <path>` and parses the result.
func getVolumeInfo(mountPoint string) (*volumeInfo, error) {
	out, err := exec.Command("diskutil", "info", "-plist", mountPoint).Output()
	if err != nil {
		return nil, fmt.Errorf("diskutil info %s: %w", mountPoint, err)
	}

	kv := parsePlistTopLevel(out)
	if kv["Error"] == "true" {
		return nil, fmt.Errorf("diskutil: %s", kv["ErrorMessage"])
	}

	info := &volumeInfo{
		VolumeName:       kv["VolumeName"],
		MountPoint:       kv["MountPoint"],
		FilesystemType:   kv["FilesystemType"],
		ContainerRef:     kv["APFSContainerReference"],
		DeviceIdentifier: kv["DeviceIdentifier"],
	}
	info.CapacityInUse, _ = strconv.ParseUint(kv["CapacityInUse"], 10, 64)
	info.ContainerSize, _ = strconv.ParseUint(kv["APFSContainerSize"], 10, 64)
	info.ContainerFree, _ = strconv.ParseUint(kv["APFSContainerFree"], 10, 64)
	return info, nil
}

// mountPointFor returns the mount point for the filesystem containing path.
func mountPointFor(path string) (string, error) {
	var stat syscall.Statfs_t
	if err := syscall.Statfs(path, &stat); err != nil {
		return "", fmt.Errorf("statfs %s: %w", path, err)
	}
	mnt := make([]byte, 0, len(stat.Mntonname))
	for _, b := range stat.Mntonname {
		if b == 0 {
			break
		}
		mnt = append(mnt, byte(b))
	}
	return string(mnt), nil
}

// hostDiskUsage returns the physical disk usage of the lnx images directory.
// If images/ is on a dedicated APFS volume (named "lnx"), returns the volume's
// CapacityInUse (accurate physical usage). Otherwise returns 0 to indicate
// no dedicated volume is configured.
func hostDiskUsage(imagesPath string) (used uint64, containerFree uint64, onVolume bool) {
	resolved, err := filepath.EvalSymlinks(imagesPath)
	if err != nil {
		return 0, 0, false
	}
	mnt, err := mountPointFor(resolved)
	if err != nil {
		return 0, 0, false
	}
	info, err := getVolumeInfo(mnt)
	if err != nil {
		return 0, 0, false
	}
	if info.FilesystemType != "apfs" {
		return 0, 0, false
	}
	if info.VolumeName != volumeName {
		return 0, 0, false
	}
	return info.CapacityInUse, info.ContainerFree, true
}

// ensureImagesDir creates the images/ directory on a dedicated APFS volume.
// Fails if volume creation fails (e.g., no admin privileges).
func ensureImagesDir(base string) error {
	imagesPath := filepath.Join(base, "images")

	// If images/ already exists (regular dir or symlink), nothing to do.
	if _, err := os.Lstat(imagesPath); err == nil {
		return nil
	}

	return createAPFSVolume(base)
}

// checkImagesVolume verifies that ~/.lnx/images/ exists and is on a
// dedicated APFS volume. Fails hard with instructions if not.
func checkImagesVolume() error {
	imagesPath := filepath.Join(lnxBase(), "images")
	if _, err := os.Stat(imagesPath); os.IsNotExist(err) {
		return fmt.Errorf("~/.lnx/images/ does not exist — run 'lnx init' to create the APFS volume")
	}

	resolved, err := filepath.EvalSymlinks(imagesPath)
	if err != nil {
		return fmt.Errorf("resolve ~/.lnx/images/: %w", err)
	}
	mnt, err := mountPointFor(resolved)
	if err != nil {
		return fmt.Errorf("stat ~/.lnx/images/: %w", err)
	}
	info, err := getVolumeInfo(mnt)
	if err != nil {
		return fmt.Errorf("get volume info for %s: %w", mnt, err)
	}
	if info.VolumeName != volumeName {
		return fmt.Errorf("~/.lnx/images/ is not on a dedicated APFS volume (found %q on %q).\n"+
			"Run 'lnx init' to create the %q volume for accurate disk usage tracking.",
			resolved, info.VolumeName, volumeName)
	}
	return nil
}

// createAPFSVolume creates an APFS volume named "lnx" on the same container
// as the base directory and symlinks base/images to the volume mount point.
func createAPFSVolume(base string) error {
	// Find the APFS container for the volume containing base.
	mnt, err := mountPointFor(base)
	if err != nil {
		return err
	}
	info, err := getVolumeInfo(mnt)
	if err != nil {
		return err
	}
	if info.FilesystemType != "apfs" || info.ContainerRef == "" {
		return fmt.Errorf("filesystem at %s is not APFS", base)
	}

	// Check if the volume already exists (mounted at /Volumes/lnx).
	volumeMountPoint := filepath.Join("/Volumes", volumeName)
	if vi, err := getVolumeInfo(volumeMountPoint); err == nil && vi.VolumeName == volumeName {
		// Volume exists, just create the symlink.
		return os.Symlink(volumeMountPoint, filepath.Join(base, "images"))
	}

	fmt.Fprintf(os.Stderr, "  creating APFS volume %q on %s...\n", volumeName, info.ContainerRef)
	cmd := exec.Command("diskutil", "apfs", "addVolume", info.ContainerRef, "APFS", volumeName)
	cmd.Stdout = os.Stderr
	cmd.Stderr = os.Stderr
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("diskutil apfs addVolume: %w", err)
	}

	// Wait for the volume to appear.
	if _, err := os.Stat(volumeMountPoint); err != nil {
		return fmt.Errorf("volume created but not mounted at %s", volumeMountPoint)
	}

	// Symlink images/ → /Volumes/lnx.
	return os.Symlink(volumeMountPoint, filepath.Join(base, "images"))
}

// parsePlistTopLevel extracts top-level key-value pairs from a plist XML.
// Only handles <string>, <integer>, <true/>, and <false/> values.
// Skips nested structures (arrays, dicts).
func parsePlistTopLevel(data []byte) map[string]string {
	result := make(map[string]string)
	decoder := xml.NewDecoder(strings.NewReader(string(data)))

	// Find the top-level <dict>.
	depth := 0
	inTopDict := false
	var currentKey string
	readingKey := false
	readingValue := false
	var valueTag string

	for {
		tok, err := decoder.Token()
		if err != nil {
			break
		}
		switch t := tok.(type) {
		case xml.StartElement:
			switch t.Name.Local {
			case "dict":
				depth++
				if depth == 1 {
					inTopDict = true
				}
			case "array":
				depth++
			case "key":
				if inTopDict && depth == 1 {
					readingKey = true
					currentKey = ""
				}
			case "string", "integer":
				if inTopDict && depth == 1 && currentKey != "" {
					readingValue = true
					valueTag = t.Name.Local
				}
			}
		case xml.EndElement:
			switch t.Name.Local {
			case "dict":
				depth--
				if depth == 0 {
					inTopDict = false
				}
			case "array":
				depth--
			case "key":
				readingKey = false
			case "string", "integer":
				readingValue = false
				valueTag = ""
			}
			// Handle <true/> and <false/> as self-closing.
		case xml.CharData:
			s := strings.TrimSpace(string(t))
			if readingKey {
				currentKey += s
			} else if readingValue && valueTag != "" {
				result[currentKey] = s
				currentKey = ""
			}
		}

		// Handle self-closing elements like <true/> and <false/>.
		if se, ok := tok.(xml.StartElement); ok {
			if inTopDict && depth == 1 && currentKey != "" {
				switch se.Name.Local {
				case "true":
					result[currentKey] = "true"
					currentKey = ""
				case "false":
					result[currentKey] = "false"
					currentKey = ""
				}
			}
		}
	}
	return result
}
