package main

import (
	"archive/tar"
	"compress/gzip"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"

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

	if err := ensureImagesDir(base); err != nil {
		return err
	}

	imgDir := imagesDir()
	if err := os.MkdirAll(imgDir, 0755); err != nil {
		return fmt.Errorf("create images dir: %w", err)
	}

	dir := instanceDir()
	if err := os.MkdirAll(dir, 0755); err != nil {
		return fmt.Errorf("create instance dir: %w", err)
	}

	kernelDest := filepath.Join(base, "vmlinuz")
	rootfsDest := filepath.Join(imgDir, "rootfs.ext4")

	// Kernel.
	if kernelFile != "" {
		if err := copyFile(kernelDest, kernelFile); err != nil {
			return fmt.Errorf("copy kernel: %w", err)
		}
	} else {
		if err := downloadKernelRelease(kernelDest); err != nil {
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

	if err := downloadFirecracker(); err != nil {
		return err
	}

	// Install the Firecracker kernel for nested VM support.
	fcKernelDest := filepath.Join(base, "vmlinuz-firecracker")
	if _, err := os.Stat(fcKernelDest); os.IsNotExist(err) {
		// Try to download from release, or copy from local repo.
		if err := downloadRelease(fcKernelDest, "vmlinuz-firecracker"); err != nil {
			fmt.Fprintf(os.Stderr, "  vmlinuz-firecracker: skipped (%v)\n", err)
		}
	} else {
		fmt.Printf("  vmlinuz-firecracker: %s (already exists)\n", fcKernelDest)
	}

	installSSHConfig()

	fmt.Println("lnx init complete")
	return nil
}

// autoInit downloads kernel and rootfs if they don't exist.
// When running nested (LNX_PARENT is set), clones from the host's
// default rootfs instead of downloading.
func autoInit() error {
	base := lnxBase()
	os.MkdirAll(base, 0755)

	if err := ensureImagesDir(base); err != nil {
		return err
	}

	imgDir := imagesDir()
	os.MkdirAll(imgDir, 0755)

	dir := instanceDir()
	os.MkdirAll(dir, 0755)

	kernelDest := filepath.Join(base, "vmlinuz")
	if err := downloadKernelRelease(kernelDest); err != nil {
		return fmt.Errorf("download kernel: %w", err)
	}

	rootfsDest := filepath.Join(imgDir, "rootfs.ext4")
	if _, err := os.Stat(rootfsDest); os.IsNotExist(err) {
		// Try to clone from an existing default rootfs first (fast, works nested).
		if src := findDefaultRootfs(); src != "" {
			fmt.Fprintf(os.Stderr, "  cloning rootfs from %s\n", src)
			if err := cloneRootfs(src, rootfsDest); err != nil {
				return fmt.Errorf("clone rootfs: %w", err)
			}
		} else {
			if err := downloadRelease(rootfsDest, "rootfs.ext4.zst"); err != nil {
				return fmt.Errorf("download rootfs: %w", err)
			}
		}
	}

	if err := downloadFirecracker(); err != nil {
		return fmt.Errorf("download firecracker: %w", err)
	}

	installSSHConfig()

	fmt.Fprintln(os.Stderr, "init complete")
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

	total := resp.ContentLength
	progress := &progressReader{r: resp.Body, total: total, label: asset}

	tmp := dest + ".tmp"
	f, err := os.Create(tmp)
	if err != nil {
		return err
	}

	// Decompress zstd if the asset is compressed.
	if filepath.Ext(asset) == ".zst" {
		zstdCmd := exec.Command("zstd", "-d", "--stdout")
		zstdCmd.Stdin = progress
		zstdCmd.Stdout = f
		zstdCmd.Stderr = os.Stderr
		if err := zstdCmd.Run(); err != nil {
			f.Close()
			os.Remove(tmp)
			return fmt.Errorf("zstd decompress failed (install zstd: brew install zstd): %w", err)
		}
		progress.finish()
		f.Close()

		// Punch holes for zero-filled blocks to make the file sparse.
		// A 4 GB rootfs with ~1 GB of data shrinks to ~1 GB on disk.
		if err := punchHoles(tmp, 64*1024); err != nil {
			os.Remove(tmp)
			return fmt.Errorf("punch holes: %w", err)
		}

		return os.Rename(tmp, dest)
	}

	var reader io.Reader = progress

	// Decompress gzip if applicable.
	if filepath.Ext(asset) == ".gz" {
		gz, err := gzip.NewReader(progress)
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
	progress.finish()
	if err := f.Close(); err != nil {
		os.Remove(tmp)
		return err
	}
	return os.Rename(tmp, dest)
}

func downloadKernelRelease(dest string) error {
	var errs []string
	for _, asset := range []string{"kernel.Image", "vmlinuz.gz"} {
		if err := downloadRelease(dest, asset); err == nil {
			return nil
		} else {
			errs = append(errs, fmt.Sprintf("%s: %v", asset, err))
		}
	}
	return fmt.Errorf("%s", strings.Join(errs, "; "))
}

// progressReader wraps an io.Reader and prints download progress to stderr.
type progressReader struct {
	r         io.Reader
	total     int64
	read      int64
	label     string
	lastPrint time.Time
}

func (p *progressReader) Read(buf []byte) (int, error) {
	n, err := p.r.Read(buf)
	p.read += int64(n)
	if time.Since(p.lastPrint) > 200*time.Millisecond {
		p.print()
		p.lastPrint = time.Now()
	}
	return n, err
}

func (p *progressReader) print() {
	readMB := float64(p.read) / (1024 * 1024)
	if p.total > 0 {
		totalMB := float64(p.total) / (1024 * 1024)
		pct := float64(p.read) * 100 / float64(p.total)
		fmt.Fprintf(os.Stderr, "\r  %s: %.1f / %.1f MB (%.0f%%)", p.label, readMB, totalMB, pct)
	} else {
		fmt.Fprintf(os.Stderr, "\r  %s: %.1f MB", p.label, readMB)
	}
}

func (p *progressReader) finish() {
	p.print()
	fmt.Fprintln(os.Stderr)
}

const firecrackerVersion = "v1.12.0"

// downloadFirecracker downloads the Firecracker binary for nested VM support.
// Always downloads the Linux arm64 binary (on macOS it's used inside the guest).
func downloadFirecracker() error {
	binDir := filepath.Join(lnxBase(), "bin")
	if err := os.MkdirAll(binDir, 0755); err != nil {
		return fmt.Errorf("create bin dir: %w", err)
	}

	fcPath := filepath.Join(binDir, "firecracker")
	if _, err := os.Stat(fcPath); err == nil {
		fmt.Printf("  firecracker: %s (already exists)\n", fcPath)
		return nil
	}

	url := fmt.Sprintf(
		"https://github.com/firecracker-microvm/firecracker/releases/download/%s/firecracker-%s-aarch64.tgz",
		firecrackerVersion, firecrackerVersion,
	)
	fmt.Printf("  downloading firecracker %s\n", firecrackerVersion)
	resp, err := http.Get(url)
	if err != nil {
		return fmt.Errorf("download firecracker: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("download firecracker: HTTP %d: %s", resp.StatusCode, url)
	}

	progress := &progressReader{r: resp.Body, total: resp.ContentLength, label: "firecracker"}
	gz, err := gzip.NewReader(progress)
	if err != nil {
		return fmt.Errorf("decode firecracker archive: %w", err)
	}
	defer gz.Close()

	tmp := fcPath + ".tmp"
	out, err := os.Create(tmp)
	if err != nil {
		return fmt.Errorf("create firecracker temp file: %w", err)
	}

	targetName := fmt.Sprintf("firecracker-%s-aarch64", firecrackerVersion)
	found := false
	tr := tar.NewReader(gz)
	for {
		hdr, err := tr.Next()
		if err == io.EOF {
			break
		}
		if err != nil {
			out.Close()
			os.Remove(tmp)
			return fmt.Errorf("read firecracker archive: %w", err)
		}
		if hdr.Typeflag != tar.TypeReg && hdr.Typeflag != tar.TypeRegA {
			continue
		}
		if filepath.Base(hdr.Name) != targetName {
			continue
		}
		if _, err := io.Copy(out, tr); err != nil {
			out.Close()
			os.Remove(tmp)
			return fmt.Errorf("extract firecracker binary: %w", err)
		}
		found = true
		break
	}
	progress.finish()
	if err := out.Close(); err != nil {
		os.Remove(tmp)
		return fmt.Errorf("close firecracker temp file: %w", err)
	}
	if !found {
		os.Remove(tmp)
		return fmt.Errorf("extract firecracker binary: %s not found in archive", targetName)
	}
	if err := os.Chmod(tmp, 0755); err != nil {
		os.Remove(tmp)
		return fmt.Errorf("chmod firecracker: %w", err)
	}
	if err := os.Rename(tmp, fcPath); err != nil {
		os.Remove(tmp)
		return fmt.Errorf("install firecracker: %w", err)
	}

	fmt.Printf("  firecracker: %s\n", fcPath)
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
