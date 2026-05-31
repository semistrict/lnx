package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"

	"github.com/spf13/cobra"
)

var syncCmd = &cobra.Command{
	Use:   "sync",
	Short: "Manage lazily-cached sync shares",
}

var syncAddCmd = &cobra.Command{
	Use:   "add <dir>",
	Short: "Add a host directory as a lazily-cached sync share",
	Long: `Add a host directory to be shared with lazy caching.
The directory is mounted read-only via virtiofs and lazily copied into the
guest's ext4 rootfs so files are served at native ext4 speed after first access.
The mount appears at the same absolute path inside the VM.
The share is persisted and restored on every boot of this instance.`,
	Args: cobra.ExactArgs(1),
	RunE: runSyncAdd,
}

var syncRemoveCmd = &cobra.Command{
	Use:   "remove <dir>",
	Short: "Remove a sync share",
	Args:  cobra.ExactArgs(1),
	RunE:  runSyncRemove,
}

var syncListCmd = &cobra.Command{
	Use:   "list",
	Short: "List sync shares",
	Args:  cobra.NoArgs,
	RunE:  runSyncList,
}

func init() {
	syncCmd.AddCommand(syncAddCmd)
	syncCmd.AddCommand(syncRemoveCmd)
	syncCmd.AddCommand(syncListCmd)
	rootCmd.AddCommand(syncCmd)
}

func syncSharesFile(dir string) string {
	return filepath.Join(dir, "sync-shares.json")
}

func loadSyncShares(dir string) []string {
	data, err := os.ReadFile(syncSharesFile(dir))
	if err != nil {
		return nil
	}
	var shares []string
	if err := json.Unmarshal(data, &shares); err != nil {
		return nil
	}
	return shares
}

func saveSyncShares(dir string, shares []string) error {
	data, err := json.MarshalIndent(shares, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(syncSharesFile(dir), data, 0644)
}

func runSyncAdd(cmd *cobra.Command, args []string) error {
	dir := instanceDir()

	path, err := filepath.Abs(args[0])
	if err != nil {
		return fmt.Errorf("resolve path: %w", err)
	}

	info, err := os.Stat(path)
	if err != nil {
		return fmt.Errorf("stat %s: %w", path, err)
	}
	if !info.IsDir() {
		return fmt.Errorf("%s is not a directory", path)
	}

	shares := loadSyncShares(dir)
	for _, s := range shares {
		if s == path {
			fmt.Printf("%s is already a sync share\n", path)
			return nil
		}
	}

	shares = append(shares, path)
	if err := saveSyncShares(dir, shares); err != nil {
		return fmt.Errorf("save sync shares: %w", err)
	}

	fmt.Printf("added sync share %s (takes effect on next boot)\n", path)
	return nil
}

func runSyncRemove(cmd *cobra.Command, args []string) error {
	dir := instanceDir()

	path, err := filepath.Abs(args[0])
	if err != nil {
		return fmt.Errorf("resolve path: %w", err)
	}

	shares := loadSyncShares(dir)
	var filtered []string
	found := false
	for _, s := range shares {
		if s == path {
			found = true
		} else {
			filtered = append(filtered, s)
		}
	}

	if !found {
		return fmt.Errorf("%s is not a sync share", path)
	}

	if err := saveSyncShares(dir, filtered); err != nil {
		return fmt.Errorf("save sync shares: %w", err)
	}

	fmt.Printf("removed sync share %s (takes effect on next boot)\n", path)
	return nil
}

func runSyncList(cmd *cobra.Command, args []string) error {
	dir := instanceDir()
	shares := loadSyncShares(dir)

	if len(shares) == 0 {
		fmt.Println("no sync shares")
		return nil
	}

	for _, s := range shares {
		fmt.Println(s)
	}
	return nil
}
