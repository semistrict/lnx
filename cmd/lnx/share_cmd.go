package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"

	"github.com/spf13/cobra"
)

var shareCmd = &cobra.Command{
	Use:   "share",
	Short: "Manage shared directories",
}

var shareAddCmd = &cobra.Command{
	Use:   "add <dir>",
	Short: "Share a host directory read-write with the VM",
	Long: `Add a host directory to be mounted read-write in the guest via virtiofs.
The directory is mounted at the same absolute path inside the VM.
The share is persisted and restored on every boot of this instance.`,
	Args: cobra.ExactArgs(1),
	RunE: runShareAdd,
}

var shareRemoveCmd = &cobra.Command{
	Use:   "remove <dir>",
	Short: "Stop sharing a directory",
	Args:  cobra.ExactArgs(1),
	RunE:  runShareRemove,
}

var shareListCmd = &cobra.Command{
	Use:   "list",
	Short: "List shared directories",
	Args:  cobra.NoArgs,
	RunE:  runShareList,
}

func init() {
	shareCmd.AddCommand(shareAddCmd)
	shareCmd.AddCommand(shareRemoveCmd)
	shareCmd.AddCommand(shareListCmd)
	rootCmd.AddCommand(shareCmd)
}

func sharesFile(dir string) string {
	return filepath.Join(dir, "shares.json")
}

func loadShares(dir string) []string {
	data, err := os.ReadFile(sharesFile(dir))
	if err != nil {
		return nil
	}
	var shares []string
	if err := json.Unmarshal(data, &shares); err != nil {
		return nil
	}
	return shares
}

func saveShares(dir string, shares []string) error {
	data, err := json.MarshalIndent(shares, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(sharesFile(dir), data, 0644)
}

func runShareAdd(cmd *cobra.Command, args []string) error {
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

	shares := loadShares(dir)
	for _, s := range shares {
		if s == path {
			fmt.Printf("%s is already shared\n", path)
			return nil
		}
	}

	shares = append(shares, path)
	if err := saveShares(dir, shares); err != nil {
		return fmt.Errorf("save shares: %w", err)
	}

	fmt.Printf("shared %s (takes effect on next boot)\n", path)
	return nil
}

func runShareRemove(cmd *cobra.Command, args []string) error {
	dir := instanceDir()

	path, err := filepath.Abs(args[0])
	if err != nil {
		return fmt.Errorf("resolve path: %w", err)
	}

	shares := loadShares(dir)
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
		return fmt.Errorf("%s is not shared", path)
	}

	if err := saveShares(dir, filtered); err != nil {
		return fmt.Errorf("save shares: %w", err)
	}

	fmt.Printf("removed %s (takes effect on next boot)\n", path)
	return nil
}

func runShareList(cmd *cobra.Command, args []string) error {
	dir := instanceDir()
	shares := loadShares(dir)

	if len(shares) == 0 {
		fmt.Println("no shared directories")
		return nil
	}

	for _, s := range shares {
		fmt.Println(s)
	}
	return nil
}
