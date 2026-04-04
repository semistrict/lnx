package main

import (
	"fmt"
	"io"
	"os"
	"path/filepath"

	"github.com/spf13/cobra"
)

var initCmd = &cobra.Command{
	Use:   "init",
	Short: "Install pre-built kernel and rootfs into ~/.lnx",
	Long: `Copies pre-built kernel (kernel.Image) and rootfs (rootfs.ext4) into ~/.lnx/.

Build the artifacts first with: make kernel rootfs`,
	RunE: runInit,
}

var (
	kernelFile string
	rootfsFile string
)

func init() {
	initCmd.Flags().StringVar(&kernelFile, "kernel", "kernel.Image", "path to pre-built kernel Image")
	initCmd.Flags().StringVar(&rootfsFile, "rootfs", "rootfs.ext4", "path to rootfs ext4 image")
	rootCmd.AddCommand(initCmd)
}

func runInit(cmd *cobra.Command, args []string) error {
	dir := lnxDir()
	if err := os.MkdirAll(dir, 0755); err != nil {
		return fmt.Errorf("create ~/.lnx: %w", err)
	}

	kernelDest := filepath.Join(dir, "vmlinuz")
	if err := copyFile(kernelDest, kernelFile); err != nil {
		return fmt.Errorf("copy kernel: %w", err)
	}
	fmt.Printf("  kernel: %s\n", kernelDest)

	rootfsDest := filepath.Join(dir, "rootfs.ext4")
	if err := copyFile(rootfsDest, rootfsFile); err != nil {
		return fmt.Errorf("copy rootfs: %w", err)
	}
	fmt.Printf("  rootfs: %s\n", rootfsDest)

	fmt.Println("lnx init complete")
	return nil
}

func copyFile(dst, src string) error {
	if _, err := os.Stat(dst); err == nil {
		fmt.Printf("  %s already exists, skipping\n", dst)
		return nil
	}

	fmt.Printf("  copying %s -> %s\n", src, dst)
	s, err := os.Open(src)
	if err != nil {
		return err
	}
	defer s.Close()

	tmp := dst + ".tmp"
	d, err := os.Create(tmp)
	if err != nil {
		return err
	}
	if _, err := io.Copy(d, s); err != nil {
		d.Close()
		os.Remove(tmp)
		return err
	}
	if err := d.Close(); err != nil {
		os.Remove(tmp)
		return err
	}
	return os.Rename(tmp, dst)
}
