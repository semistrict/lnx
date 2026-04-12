//go:build linux

package main

import "os"

// hostDiskUsage is not supported on Linux (no APFS).
func hostDiskUsage(imagesPath string) (used uint64, containerFree uint64, onVolume bool) {
	return 0, 0, false
}

// ensureImagesDir creates the images/ directory as a regular directory on Linux.
func ensureImagesDir(base string) error {
	return os.MkdirAll(base+"/images", 0755)
}

// checkImagesVolume is a no-op on Linux (no APFS volumes).
func checkImagesVolume() error {
	return nil
}
