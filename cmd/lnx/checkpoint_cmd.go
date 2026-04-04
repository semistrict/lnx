package main

import (
	"fmt"
	"os"
	"path/filepath"
	"sort"

	"github.com/spf13/cobra"
)

var checkpointCmd = &cobra.Command{
	Use:   "checkpoints",
	Short: "Manage rootfs checkpoints",
}

var checkpointListCmd = &cobra.Command{
	Use:   "list",
	Short: "List available checkpoints",
	RunE:  runCheckpointList,
}

func init() {
	checkpointCmd.AddCommand(checkpointListCmd)
	rootCmd.AddCommand(checkpointCmd)
}

func runCheckpointList(cmd *cobra.Command, args []string) error {
	dir := filepath.Join(lnxDir(), "checkpoints")

	entries, err := os.ReadDir(dir)
	if err != nil {
		if os.IsNotExist(err) {
			fmt.Println("no checkpoints")
			return nil
		}
		return err
	}

	// Filter to .ext4 files, sort by name (timestamp-based, so chronological).
	var checkpoints []os.DirEntry
	for _, e := range entries {
		if !e.IsDir() && filepath.Ext(e.Name()) == ".ext4" {
			checkpoints = append(checkpoints, e)
		}
	}
	sort.Slice(checkpoints, func(i, j int) bool {
		return checkpoints[i].Name() < checkpoints[j].Name()
	})

	if len(checkpoints) == 0 {
		fmt.Println("no checkpoints")
		return nil
	}

	for _, e := range checkpoints {
		info, err := e.Info()
		if err != nil {
			continue
		}
		sizeMB := float64(info.Size()) / 1024 / 1024
		fmt.Printf("%-40s  %7.1f MB\n", e.Name(), sizeMB)
	}
	return nil
}
