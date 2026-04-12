package main

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/spf13/cobra"
)

var dockerCmd = &cobra.Command{
	Use:   "docker",
	Short: "Run OCI container images in lnx VMs",
}

var dockerRunCmd = &cobra.Command{
	Use:   "run IMAGE[:TAG]",
	Short: "Pull and run an OCI container image",
	Args:  cobra.ExactArgs(1),
	RunE:  runDockerRun,
}

func init() {
	dockerCmd.AddCommand(dockerRunCmd)
	rootCmd.AddCommand(dockerCmd)
}

// ociDir returns the OCI storage directory on the APFS volume.
func ociDir() string {
	return filepath.Join(lnxBase(), "images", "_oci")
}

func ociBlobDir() string {
	return filepath.Join(ociDir(), "blobs", "sha256")
}

func ociLayerDir() string {
	return filepath.Join(ociDir(), "layers")
}

func runDockerRun(cmd *cobra.Command, args []string) error {
	imageRef := args[0]
	if !strings.Contains(imageRef, ":") {
		imageRef += ":latest"
	}

	// Ensure storage directories exist.
	for _, d := range []string{ociBlobDir(), ociLayerDir()} {
		if err := os.MkdirAll(d, 0755); err != nil {
			return fmt.Errorf("create OCI directory: %w", err)
		}
	}

	// Pull image and cache layer blobs.
	fmt.Fprintf(os.Stderr, "pulling %s...\n", imageRef)
	img, err := pullImage(imageRef)
	if err != nil {
		return fmt.Errorf("pull image: %w", err)
	}

	// Build cumulative layer ext4 snapshots (pure Go, no VM needed).
	fmt.Fprintf(os.Stderr, "building layers...\n")
	finalLayerPath, err := buildLayers(img)
	if err != nil {
		return fmt.Errorf("build layers: %w", err)
	}

	// Create instance from the final layer.
	inst := instanceNameFromRef(imageRef)
	fmt.Fprintf(os.Stderr, "creating instance %q...\n", inst)
	if err := createInstanceFromLayer(inst, finalLayerPath); err != nil {
		return fmt.Errorf("create instance: %w", err)
	}

	// Boot the instance.
	instanceName = inst
	instanceFlag = true

	runArgs := img.defaultCmd()
	if len(runArgs) == 0 {
		runArgs = []string{"/bin/sh"}
	}

	exitCode, err := runVM(runArgs)
	if err != nil {
		return err
	}
	os.Exit(exitCode)
	return nil
}

// instanceNameFromRef derives an instance name from a Docker image reference.
func instanceNameFromRef(ref string) string {
	name := ref
	if i := strings.LastIndex(name, "/"); i >= 0 {
		name = name[i+1:]
	}
	name = strings.ReplaceAll(name, ":", "-")
	name = strings.ReplaceAll(name, ".", "-")
	return name
}

// createInstanceFromLayer creates a new instance by APFS-cloning the layer ext4.
func createInstanceFromLayer(name string, layerPath string) error {
	imgDir := imagesDirFor(name)
	if err := os.MkdirAll(imgDir, 0755); err != nil {
		return err
	}

	instDir := instanceDirFor(name)
	if err := os.MkdirAll(instDir, 0755); err != nil {
		return err
	}

	dst := filepath.Join(imgDir, "rootfs.ext4")
	if _, err := os.Stat(dst); err == nil {
		os.Remove(dst)
	}
	return cloneRootfs(layerPath, dst)
}
