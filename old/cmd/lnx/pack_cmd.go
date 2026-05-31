//go:build darwin

package main

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"

	"github.com/semistrict/lnx/internal/pack"
	"github.com/spf13/cobra"
)

// virtualizationEntitlements is the entitlements plist required to use
// Apple Virtualization.framework. Written to a tempfile for codesigning.
const virtualizationEntitlements = `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>com.apple.security.virtualization</key>
	<true/>
</dict>
</plist>`

var packOutput string
var packKernel string
var packRootfs string

var packCmd = &cobra.Command{
	Use:   "pack -o BIN CMD [ARGS...]",
	Short: "Create a self-contained binary that runs a fixed command in a VM",
	Long: `Pack creates a single executable that embeds the kernel and rootfs
and behaves like:

  lnx --instance INSTANCE CMD [ARGS...]

On first run the binary extracts the kernel and rootfs to a cache under
~/.lnx/packed-cache/ and boots an ephemeral VM. No lnx installation is
required on the target machine.`,
	Args: cobra.MinimumNArgs(1),
	RunE: func(cmd *cobra.Command, args []string) error {
		if packOutput == "" {
			return fmt.Errorf("--output / -o is required")
		}
		kernelPath := packKernel
		if kernelPath == "" {
			kernelPath = filepath.Join(lnxBase(), "vmlinuz")
		}
		rootfsPath := packRootfs
		if rootfsPath == "" {
			rootfsPath = resolveRootfsPath()
		}
		return runPack(instanceName, args, kernelPath, rootfsPath, packOutput)
	},
}

func init() {
	packCmd.Flags().StringVarP(&packOutput, "output", "o", "", "output binary path (required)")
	packCmd.Flags().StringVar(&packKernel, "kernel", "", "kernel image path (default: ~/.lnx/vmlinuz)")
	packCmd.Flags().StringVar(&packRootfs, "rootfs", "", "rootfs ext4 path (default: instance rootfs)")
	rootCmd.AddCommand(packCmd)
}

func runPack(instance string, args []string, kernelPath, rootfsPath, output string) error {
	for _, p := range []string{kernelPath, rootfsPath} {
		if _, err := os.Stat(p); err != nil {
			return fmt.Errorf("%s: %w", p, err)
		}
	}

	self, err := os.Executable()
	if err != nil {
		return fmt.Errorf("find executable: %w", err)
	}

	// Compress kernel and rootfs to temp files.
	tmp, err := os.MkdirTemp("", "lnx-pack-*")
	if err != nil {
		return err
	}
	defer os.RemoveAll(tmp)

	fmt.Fprintf(os.Stderr, "compressing kernel (%s)...\n", humanFileSize(kernelPath))
	kernelComp := filepath.Join(tmp, "kernel.zst")
	kernelSHA, kernelCompSize, err := pack.CompressZstdFile(kernelPath, kernelComp, nil)
	if err != nil {
		return fmt.Errorf("compress kernel: %w", err)
	}
	fmt.Fprintf(os.Stderr, "  %s → %s\n", humanFileSize(kernelPath), humanFileSize(kernelComp))

	fmt.Fprintf(os.Stderr, "compressing rootfs (%s)...\n", humanFileSize(rootfsPath))
	rootfsComp := filepath.Join(tmp, "rootfs.zst")
	rootfsSHA, rootfsCompSize, err := pack.CompressZstdFile(rootfsPath, rootfsComp, nil)
	if err != nil {
		return fmt.Errorf("compress rootfs: %w", err)
	}
	fmt.Fprintf(os.Stderr, "  %s → %s\n", humanFileSize(rootfsPath), humanFileSize(rootfsComp))

	cfg := &pack.Config{
		Instance:       instance,
		Args:           args,
		KernelCompSize: kernelCompSize,
		RootfsCompSize: rootfsCompSize,
		KernelSHA256:   kernelSHA,
		RootfsSHA256:   rootfsSHA,
	}

	// Build the blob: [kernel.zst][rootfs.zst][json][u64 json_len]
	blob, err := pack.BuildBlob(kernelComp, rootfsComp, cfg)
	if err != nil {
		return fmt.Errorf("build blob: %w", err)
	}

	// Read the source binary (the lnx executable itself).
	srcBin, err := os.ReadFile(self)
	if err != nil {
		return fmt.Errorf("read source binary: %w", err)
	}

	// Inject the blob into the __LNX,__lnxpack Mach-O section.
	fmt.Fprintln(os.Stderr, "injecting into Mach-O section...")
	outBin, err := machoInjectSection(srcBin, blob)
	if err != nil {
		return fmt.Errorf("inject section: %w", err)
	}

	// Write the modified binary.
	tmpOut := output + ".tmp"
	if err := os.WriteFile(tmpOut, outBin, 0755); err != nil {
		return fmt.Errorf("write binary: %w", err)
	}

	// Strip old signature and re-sign with virtualization entitlement.
	if err := codesignRemove(tmpOut); err != nil {
		os.Remove(tmpOut)
		return fmt.Errorf("strip signature: %w", err)
	}
	fmt.Fprintln(os.Stderr, "signing...")
	if err := codesignWithVirtualization(tmpOut); err != nil {
		os.Remove(tmpOut)
		return fmt.Errorf("codesign: %w", err)
	}

	if err := os.Rename(tmpOut, output); err != nil {
		os.Remove(tmpOut)
		return fmt.Errorf("rename: %w", err)
	}

	fi, _ := os.Stat(output)
	fmt.Fprintf(os.Stderr, "packed: %s (%s, instance=%q, cmd=%v)\n",
		output, humanBytes(fi.Size()), instance, args)
	return nil
}

// codesignRemove strips the code signature from a binary.
func codesignRemove(path string) error {
	cmd := exec.Command("codesign", "--remove-signature", path)
	out, err := cmd.CombinedOutput()
	if err != nil {
		return fmt.Errorf("%w: %s", err, out)
	}
	return nil
}

// codesignWithVirtualization signs a binary with the virtualization entitlement.
func codesignWithVirtualization(path string) error {
	entFile, err := os.CreateTemp("", "lnx-entitlements-*.plist")
	if err != nil {
		return err
	}
	defer os.Remove(entFile.Name())
	if _, err := entFile.WriteString(virtualizationEntitlements); err != nil {
		entFile.Close()
		return err
	}
	entFile.Close()

	cmd := exec.Command("codesign",
		"--entitlements", entFile.Name(),
		"--force",
		"-s", "-",
		path,
	)
	out, err := cmd.CombinedOutput()
	if err != nil {
		return fmt.Errorf("%w: %s", err, out)
	}
	return nil
}

// humanFileSize returns a human-readable size string for a file.
func humanFileSize(path string) string {
	fi, err := os.Stat(path)
	if err != nil {
		return "?"
	}
	return humanBytes(fi.Size())
}

// humanBytes returns a human-readable byte count.
func humanBytes(n int64) string {
	switch {
	case n >= 1<<30:
		return fmt.Sprintf("%.1f GB", float64(n)/(1<<30))
	case n >= 1<<20:
		return fmt.Sprintf("%.1f MB", float64(n)/(1<<20))
	case n >= 1<<10:
		return fmt.Sprintf("%.1f KB", float64(n)/(1<<10))
	default:
		return fmt.Sprintf("%d B", n)
	}
}
