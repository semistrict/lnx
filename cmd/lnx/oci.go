package main

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/semistrict/lnx/internal/lnxoci"
)

func ociDir() string {
	return filepath.Join(dockerImagesDir(), "_oci")
}

func ociBlobDir() string {
	return filepath.Join(ociDir(), "blobs", "sha256")
}

func ociLayerDir() string {
	return filepath.Join(ociDir(), "layers")
}

// ensureOCIRootfs pulls an OCI image (if needed), builds its layers into an
// ext4 rootfs, and returns the path to the cached base rootfs.
func ensureOCIRootfs(imageRef string) (string, error) {
	if !strings.Contains(imageRef, ":") {
		imageRef += ":latest"
	}

	inst := lnxoci.SlugFromRef(imageRef)
	baseRootfs := filepath.Join(dockerImageDirFor(inst), "rootfs.ext4")

	if _, err := os.Stat(baseRootfs); err == nil {
		return baseRootfs, nil
	}

	blobDir := ociBlobDir()
	layerDir := ociLayerDir()
	for _, d := range []string{blobDir, layerDir} {
		if err := os.MkdirAll(d, 0755); err != nil {
			return "", fmt.Errorf("create OCI directory: %w", err)
		}
	}

	fmt.Fprintf(os.Stderr, "pulling %s...\n", imageRef)
	img, err := lnxoci.Pull(imageRef, blobDir)
	if err != nil {
		return "", fmt.Errorf("pull image: %w", err)
	}

	fmt.Fprintf(os.Stderr, "building layers...\n")
	finalLayerPath, err := lnxoci.BuildLayers(img, blobDir, layerDir)
	if err != nil {
		return "", fmt.Errorf("build layers: %w", err)
	}

	if err := os.MkdirAll(dockerImageDirFor(inst), 0755); err != nil {
		return "", fmt.Errorf("create image dir: %w", err)
	}
	if err := cloneRootfs(finalLayerPath, baseRootfs); err != nil {
		return "", fmt.Errorf("create image rootfs: %w", err)
	}
	writeDefaultCmd(inst, img.DefaultCmd())
	writeImageMeta(inst, imageMeta{ExposedPorts: img.ExposedPorts()})

	return baseRootfs, nil
}
