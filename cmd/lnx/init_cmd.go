package main

import (
	"compress/gzip"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"

	"github.com/spf13/cobra"
)

const defaultImageVersion = "images-v0.1.0"

var initCmd = &cobra.Command{
	Use:   "init",
	Short: "Download and install kernel and rootfs",
	Long: `Downloads pre-built kernel and rootfs from GitHub releases, or copies local files.

By default, fetches from:
  https://github.com/semistrict/lnx/releases/tag/` + defaultImageVersion + `

Use --kernel and --rootfs to provide local files instead.`,
	RunE: runInit,
}

var (
	kernelFile string
	rootfsFile string
)

func init() {
	initCmd.Flags().StringVar(&kernelFile, "kernel", "", "path to local kernel Image (skip download)")
	initCmd.Flags().StringVar(&rootfsFile, "rootfs", "", "path to local rootfs ext4 image (skip download)")
	rootCmd.AddCommand(initCmd)
}

func runInit(cmd *cobra.Command, args []string) error {
	base := lnxBase()
	if err := os.MkdirAll(base, 0755); err != nil {
		return fmt.Errorf("create ~/.lnx: %w", err)
	}

	dir := instanceDir()
	if err := os.MkdirAll(dir, 0755); err != nil {
		return fmt.Errorf("create instance dir: %w", err)
	}

	kernelDest := filepath.Join(base, "vmlinuz")
	rootfsDest := filepath.Join(dir, "rootfs.ext4")

	// Kernel.
	if kernelFile != "" {
		if err := copyFile(kernelDest, kernelFile); err != nil {
			return fmt.Errorf("copy kernel: %w", err)
		}
	} else {
		if err := downloadRelease(kernelDest, "kernel.Image"); err != nil {
			return fmt.Errorf("download kernel: %w", err)
		}
	}
	fmt.Printf("  kernel: %s\n", kernelDest)

	// Rootfs.
	if rootfsFile != "" {
		if err := copyFile(rootfsDest, rootfsFile); err != nil {
			return fmt.Errorf("copy rootfs: %w", err)
		}
	} else {
		if err := downloadRelease(rootfsDest, "rootfs.ext4.zst"); err != nil {
			return fmt.Errorf("download rootfs: %w", err)
		}
	}
	fmt.Printf("  rootfs: %s\n", rootfsDest)

	fmt.Println("lnx init complete")
	return nil
}

func downloadRelease(dest, asset string) error {
	if _, err := os.Stat(dest); err == nil {
		fmt.Printf("  %s already exists, skipping\n", dest)
		return nil
	}

	url := fmt.Sprintf("https://github.com/semistrict/lnx/releases/download/%s/%s", defaultImageVersion, asset)
	fmt.Printf("  downloading %s\n", url)

	resp, err := http.Get(url)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("HTTP %d: %s", resp.StatusCode, url)
	}

	tmp := dest + ".tmp"
	f, err := os.Create(tmp)
	if err != nil {
		return err
	}

	var reader io.Reader = resp.Body

	// Decompress zstd if the asset is compressed.
	if filepath.Ext(asset) == ".zst" {
		// Use zstd CLI for decompression (avoids adding a Go zstd dependency).
		zstdCmd := exec.Command("zstd", "-d", "--stdout")
		zstdCmd.Stdin = resp.Body
		zstdCmd.Stdout = f
		zstdCmd.Stderr = os.Stderr
		if err := zstdCmd.Run(); err != nil {
			f.Close()
			os.Remove(tmp)
			// Fall back to trying uncompressed download.
			return fmt.Errorf("zstd decompress failed (install zstd: brew install zstd): %w", err)
		}
		f.Close()
		return os.Rename(tmp, dest)
	}

	// Decompress gzip if applicable.
	if filepath.Ext(asset) == ".gz" {
		gz, err := gzip.NewReader(resp.Body)
		if err != nil {
			f.Close()
			os.Remove(tmp)
			return err
		}
		reader = gz
		defer gz.Close()
	}

	if _, err := io.Copy(f, reader); err != nil {
		f.Close()
		os.Remove(tmp)
		return err
	}
	if err := f.Close(); err != nil {
		os.Remove(tmp)
		return err
	}
	return os.Rename(tmp, dest)
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
