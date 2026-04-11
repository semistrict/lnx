package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"github.com/spf13/cobra"

	"github.com/semistrict/lnx"
)

var checkpointCmd = &cobra.Command{
	Use:   "checkpoints",
	Short: "Manage rootfs checkpoints",
}

var (
	cpMemory      bool
	cpDescription string
	cpTags        []string
)

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

var checkpointRestoreCmd = &cobra.Command{
	Use:   "restore <name>",
	Short: "Restore from a checkpoint",
	Args:  cobra.ExactArgs(1),
	RunE:  runCheckpointRestore,
}

var checkpointDeleteCmd = &cobra.Command{
	Use:   "delete <name>",
	Short: "Delete a checkpoint",
	Args:  cobra.ExactArgs(1),
	RunE:  runCheckpointDelete,
}

func init() {
	checkpointCreateCmd.Flags().BoolVar(&cpMemory, "memory", false, "Create a memory checkpoint (hibernate + clone rootfs + swap)")
	checkpointCreateCmd.Flags().StringVar(&cpDescription, "description", "", "Description for the checkpoint")
	checkpointCreateCmd.Flags().StringArrayVar(&cpTags, "tag", nil, "Tag for the checkpoint (can be repeated)")

	checkpointCmd.AddCommand(checkpointListCmd, checkpointCreateCmd, checkpointRestoreCmd, checkpointDeleteCmd)
	rootCmd.AddCommand(checkpointCmd)
}

func runCheckpointCreate(cmd *cobra.Command, args []string) error {
	name := ""
	if len(args) == 1 {
		name = args[0]
	}

	if cpMemory {
		return runMemoryCheckpointCreate(name)
	}

	cpPath, err := createInstanceCheckpoint(instanceDir(), qualifiedInstanceName(), name)
	if err != nil {
		return err
	}

	fmt.Printf("created checkpoint %q\n", filepath.Base(cpPath))
	return nil
}

func runMemoryCheckpointCreate(name string) error {
	instName := qualifiedInstanceName()

	if isInstanceRunning(instName) {
		return createMemoryCheckpointViaAPI(instName, name, cpDescription, cpTags)
	}

	// VM not running — can't create a memory checkpoint without hibernating.
	return fmt.Errorf("VM must be running to create a memory checkpoint (use without --memory for a disk-only checkpoint)")
}

func createMemoryCheckpointViaAPI(instName, name, description string, tags []string) error {
	body, err := json.Marshal(map[string]any{
		"name":        name,
		"description": description,
		"tags":        tags,
	})
	if err != nil {
		return fmt.Errorf("marshal request: %w", err)
	}

	resp, err := apiClientFor(instName).Post("http://localhost/checkpoint/memory", "application/json", bytes.NewReader(body))
	if err != nil {
		if isNoVM(err) {
			return noVMError()
		}
		return err
	}
	defer resp.Body.Close()

	if resp.StatusCode/100 != 2 {
		data, _ := io.ReadAll(resp.Body)
		msg := strings.TrimSpace(string(data))
		if msg == "" {
			msg = resp.Status
		}
		return fmt.Errorf("%s", msg)
	}

	var payload struct {
		Name   string `json:"name"`
		Status string `json:"status"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&payload); err != nil && err != io.EOF {
		return fmt.Errorf("decode response: %w", err)
	}

	fmt.Printf("created memory checkpoint %q\n", name)
	return nil
}

func runCheckpointRestore(cmd *cobra.Command, args []string) error {
	name := args[0]
	instName := qualifiedInstanceName()
	instPath := instanceDir()

	if isInstanceRunning(instName) {
		return restoreCheckpointViaAPI(instName, name)
	}

	// VM not running — restore directly.
	cpDir := filepath.Join(instPath, "checkpoints")
	rootfsPath := filepath.Join(instPath, "rootfs.ext4")
	swapPath := filepath.Join(instPath, "swap.img")

	// Check if it's a memory checkpoint.
	metaPath := filepath.Join(cpDir, name, "metadata.json")
	if _, err := os.Stat(metaPath); err == nil {
		lock, err := lnx.LockRootfs(rootfsPath)
		if err != nil {
			return fmt.Errorf("lock rootfs: %w", err)
		}
		defer lock.Unlock()

		if err := lnx.RestoreMemoryCheckpoint(cpDir, name, rootfsPath, swapPath); err != nil {
			return err
		}
		// Write hibernated marker so next boot resumes from the checkpoint.
		sockDir := instPath
		os.WriteFile(filepath.Join(sockDir, "hibernated"), []byte("1"), 0644)
		fmt.Printf("restored memory checkpoint %q (next boot will resume from hibernate)\n", name)
		return nil
	}

	// Legacy disk-only checkpoint — just replace rootfs.
	ext4Name := name
	if !strings.HasSuffix(ext4Name, ".ext4") {
		ext4Name += ".ext4"
	}
	cpPath := filepath.Join(cpDir, ext4Name)
	if _, err := os.Stat(cpPath); err != nil {
		return fmt.Errorf("checkpoint %q not found", name)
	}

	lock, err := lnx.LockRootfs(rootfsPath)
	if err != nil {
		return fmt.Errorf("lock rootfs: %w", err)
	}
	defer lock.Unlock()

	if err := os.Remove(rootfsPath); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("remove rootfs: %w", err)
	}
	if err := lnx.CloneFile(cpPath, rootfsPath); err != nil {
		return fmt.Errorf("clone checkpoint rootfs: %w", err)
	}

	fmt.Printf("restored disk checkpoint %q (next boot will cold start)\n", name)
	return nil
}

func restoreCheckpointViaAPI(instName, name string) error {
	body, err := json.Marshal(map[string]string{"name": name})
	if err != nil {
		return fmt.Errorf("marshal request: %w", err)
	}

	resp, err := apiClientFor(instName).Post("http://localhost/checkpoint/restore", "application/json", bytes.NewReader(body))
	if err != nil {
		if isNoVM(err) {
			return noVMError()
		}
		return err
	}
	defer resp.Body.Close()

	if resp.StatusCode/100 != 2 {
		data, _ := io.ReadAll(resp.Body)
		msg := strings.TrimSpace(string(data))
		if msg == "" {
			msg = resp.Status
		}
		return fmt.Errorf("%s", msg)
	}

	fmt.Printf("restored checkpoint %q (next boot will resume from hibernate)\n", name)
	return nil
}

func runCheckpointList(cmd *cobra.Command, args []string) error {
	dir := filepath.Join(instanceDir(), "checkpoints")

	checkpoints, err := lnx.ListCheckpoints(dir)
	if err != nil {
		return err
	}

	if len(checkpoints) == 0 {
		fmt.Println("no checkpoints")
		return nil
	}

	t := newTable("NAME", "TYPE", "DESCRIPTION", "TAGS", "CREATED")
	for _, cp := range checkpoints {
		tags := strings.Join(cp.Tags, ", ")
		created := cp.CreatedAt.Format("2006-01-02 15:04:05")
		t.Row(cp.Name, string(cp.Type), cp.Description, tags, created)
	}
	fmt.Println(t)
	return nil
}

func runCheckpointDelete(cmd *cobra.Command, args []string) error {
	name := args[0]
	dir := filepath.Join(instanceDir(), "checkpoints")

	if err := lnx.DeleteCheckpoint(dir, name); err != nil {
		return err
	}

	fmt.Printf("deleted checkpoint %q\n", name)
	return nil
}
