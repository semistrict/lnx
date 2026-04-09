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

var checkpointCreateCmd = &cobra.Command{
	Use:   "create [name]",
	Short: "Create a checkpoint for the current instance",
	Args:  cobra.MaximumNArgs(1),
	RunE:  runCheckpointCreate,
}

func init() {
	checkpointCmd.AddCommand(checkpointListCmd, checkpointCreateCmd)
	rootCmd.AddCommand(checkpointCmd)
}

func runCheckpointCreate(cmd *cobra.Command, args []string) error {
	name := ""
	if len(args) == 1 {
		name = args[0]
	}

	cpPath, err := createInstanceCheckpoint(instanceDir(), qualifiedInstanceName(), name)
	if err != nil {
		return err
	}

	fmt.Printf("created checkpoint %q\n", filepath.Base(cpPath))
	return nil
}

func runCheckpointList(cmd *cobra.Command, args []string) error {
	dir := filepath.Join(instanceDir(), "checkpoints")

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

	t := newTable("NAME", "SIZE")
	for _, e := range checkpoints {
		info, err := e.Info()
		if err != nil {
			continue
		}
		sizeMB := float64(info.Size()) / 1024 / 1024
		t.Row(e.Name(), fmt.Sprintf("%.1f MB", sizeMB))
	}
	fmt.Println(t)
	return nil
}
