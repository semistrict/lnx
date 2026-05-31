package main

import (
	"fmt"
	"os"
	"strconv"
	"strings"

	"github.com/spf13/cobra"
)

var diskCmd = &cobra.Command{
	Use:   "disk",
	Short: "Manage VM disk",
}

var diskGrowCmd = &cobra.Command{
	Use:   "grow <size>",
	Short: "Grow the rootfs image to the given size",
	Long: `Grow the rootfs ext4 image to the given size.

Size can be specified as:
  lnx disk grow 8G       set total size to 8 GiB
  lnx disk grow 16GB     set total size to 16 GB (decimal)
  lnx disk grow +2G      grow by 2 GiB from current size

The filesystem is resized automatically on next boot (resize2fs).
The instance must not be running.`,
	Args: cobra.ExactArgs(1),
	RunE: runDiskGrow,
}

func init() {
	diskCmd.AddCommand(diskGrowCmd)
	rootCmd.AddCommand(diskCmd)
}

func runDiskGrow(cmd *cobra.Command, args []string) error {
	rootfs := resolveRootfsPath()

	info, err := os.Stat(rootfs)
	if err != nil {
		return fmt.Errorf("stat rootfs: %w", err)
	}
	currentSize := info.Size()

	targetSize, err := parseSize(args[0], currentSize)
	if err != nil {
		return err
	}

	if targetSize <= currentSize {
		fmt.Printf("rootfs is already %s (requested %s)\n", formatSize(currentSize), formatSize(targetSize))
		return nil
	}

	f, err := os.OpenFile(rootfs, os.O_RDWR, 0)
	if err != nil {
		return fmt.Errorf("open rootfs: %w", err)
	}
	defer f.Close()

	if err := f.Truncate(targetSize); err != nil {
		return fmt.Errorf("truncate: %w", err)
	}

	fmt.Printf("rootfs: %s → %s\n", formatSize(currentSize), formatSize(targetSize))
	fmt.Println("filesystem will be resized on next boot")
	return nil
}

// parseSize parses a size string like "8G", "16GB", "+2G".
func parseSize(s string, current int64) (int64, error) {
	relative := false
	if strings.HasPrefix(s, "+") {
		relative = true
		s = s[1:]
	}

	// Find where the number ends and the unit begins.
	i := 0
	for i < len(s) && (s[i] == '.' || (s[i] >= '0' && s[i] <= '9')) {
		i++
	}
	if i == 0 {
		return 0, fmt.Errorf("invalid size: %q", s)
	}

	num, err := strconv.ParseFloat(s[:i], 64)
	if err != nil {
		return 0, fmt.Errorf("invalid size: %q", s)
	}

	unit := strings.ToUpper(strings.TrimSpace(s[i:]))
	var multiplier float64
	switch unit {
	case "", "B":
		multiplier = 1
	case "K", "KB", "KIB":
		multiplier = 1024
	case "M", "MB", "MIB":
		multiplier = 1024 * 1024
	case "G", "GIB":
		multiplier = 1024 * 1024 * 1024
	case "GB":
		multiplier = 1e9
	case "T", "TIB":
		multiplier = 1024 * 1024 * 1024 * 1024
	case "TB":
		multiplier = 1e12
	default:
		return 0, fmt.Errorf("unknown unit: %q", unit)
	}

	size := int64(num * multiplier)
	if relative {
		size += current
	}
	return size, nil
}

func formatSize(b int64) string {
	const gib = 1024 * 1024 * 1024
	if b >= gib {
		g := float64(b) / float64(gib)
		if g == float64(int64(g)) {
			return fmt.Sprintf("%dG", int64(g))
		}
		return fmt.Sprintf("%.1fG", g)
	}
	const mib = 1024 * 1024
	return fmt.Sprintf("%dM", b/mib)
}
