package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"

	lnx "github.com/semistrict/lnx"
	"github.com/spf13/cobra"
)

var checkpointCRIU bool

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

var checkpointRestoreCmd = &cobra.Command{
	Use:   "restore <name>",
	Short: "Restore a checkpoint (requires VM to be stopped)",
	Args:  cobra.ExactArgs(1),
	RunE:  runCheckpointRestore,
}

func init() {
	checkpointCreateCmd.Flags().BoolVar(&checkpointCRIU, "criu", false, "CRIU checkpoint: dump process state + clone rootfs and CRIU volume")
	checkpointCmd.AddCommand(checkpointListCmd, checkpointCreateCmd, checkpointRestoreCmd)
	rootCmd.AddCommand(checkpointCmd)
}

func runCheckpointCreate(cmd *cobra.Command, args []string) error {
	name := ""
	if len(args) == 1 {
		name = args[0]
	}

	if checkpointCRIU {
		if name == "" {
			return fmt.Errorf("--criu requires a checkpoint name")
		}
		return createCRIUCheckpoint(name)
	}

	cpPath, err := createInstanceCheckpoint(filepath.Dir(resolveRootfsPath()), qualifiedInstanceName(), name)
	if err != nil {
		return err
	}

	fmt.Printf("created checkpoint %q\n", filepath.Base(cpPath))
	return nil
}

func createCRIUCheckpoint(name string) error {
	instanceName := qualifiedInstanceName()
	if !isInstanceRunning(instanceName) {
		return fmt.Errorf("VM must be running for CRIU checkpoints")
	}

	client := apiClientFor(instanceName)
	body, err := json.Marshal(map[string]string{"name": name})
	if err != nil {
		return err
	}
	resp, err := client.Post("http://localhost/criu/checkpoint", "application/json", bytes.NewReader(body))
	if err != nil {
		if isNoVM(err) {
			return noVMError()
		}
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode/100 != 2 {
		data, _ := io.ReadAll(resp.Body)
		return fmt.Errorf("%s", strings.TrimSpace(string(data)))
	}

	var result struct {
		Path string `json:"path"`
	}
	json.NewDecoder(resp.Body).Decode(&result)
	fmt.Printf("created CRIU checkpoint %q at %s\n", name, result.Path)
	return nil
}

func runCheckpointRestore(cmd *cobra.Command, args []string) error {
	name := args[0]
	instanceName := qualifiedInstanceName()

	if isInstanceRunning(instanceName) {
		return fmt.Errorf("stop the VM before restoring (lnx stop --shutdown)")
	}

	imgDir := filepath.Dir(resolveRootfsPath())

	// Check for CRIU checkpoint (directory with rootfs.ext4 + criu.ext4).
	criuDir := filepath.Join(imgDir, "checkpoints", name)
	if _, err := os.Stat(filepath.Join(criuDir, "rootfs.ext4")); err == nil {
		return restoreCRIUCheckpoint(imgDir, criuDir, name)
	}

	// Fall back to disk-only checkpoint (.ext4 file).
	return restoreDiskCheckpoint(imgDir, name)
}

func restoreCRIUCheckpoint(imgDir, criuDir, name string) error {
	rootfsPath := filepath.Join(imgDir, "rootfs.ext4")
	criuPath := filepath.Join(imgDir, "criu.ext4")

	// Lock rootfs to prevent concurrent access.
	lock, err := lnx.LockRootfs(rootfsPath)
	if err != nil {
		return fmt.Errorf("lock rootfs: %w", err)
	}
	defer lock.Unlock()

	// Replace rootfs with checkpoint clone.
	if err := os.Remove(rootfsPath); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("remove rootfs: %w", err)
	}
	if err := cloneRootfs(filepath.Join(criuDir, "rootfs.ext4"), rootfsPath); err != nil {
		return fmt.Errorf("clone rootfs: %w", err)
	}

	// Replace CRIU volume with checkpoint clone.
	if err := os.Remove(criuPath); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("remove criu volume: %w", err)
	}
	if err := cloneRootfs(filepath.Join(criuDir, "criu.ext4"), criuPath); err != nil {
		return fmt.Errorf("clone criu volume: %w", err)
	}

	fmt.Printf("restored CRIU checkpoint %q\n", name)
	fmt.Println("boot the VM to restore processes")
	return nil
}

func restoreDiskCheckpoint(imgDir, name string) error {
	cpPath, err := resolveNamedCheckpoint(imgDir, name)
	if err != nil {
		return err
	}

	rootfsPath := filepath.Join(imgDir, "rootfs.ext4")

	// Lock rootfs.
	lock, err := lnx.LockRootfs(rootfsPath)
	if err != nil {
		return fmt.Errorf("lock rootfs: %w", err)
	}
	defer lock.Unlock()

	if err := os.Remove(rootfsPath); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("remove rootfs: %w", err)
	}
	if err := cloneRootfs(cpPath, rootfsPath); err != nil {
		return fmt.Errorf("clone checkpoint: %w", err)
	}
	fmt.Printf("restored disk checkpoint %q\n", name)
	return nil
}

func runCheckpointList(cmd *cobra.Command, args []string) error {
	dir := filepath.Join(filepath.Dir(resolveRootfsPath()), "checkpoints")

	entries, err := os.ReadDir(dir)
	if err != nil {
		if os.IsNotExist(err) {
			fmt.Println("no checkpoints")
			return nil
		}
		return err
	}

	type cpEntry struct {
		name    string
		size    string
		cpType  string
	}
	var checkpoints []cpEntry

	for _, e := range entries {
		if e.IsDir() {
			// CRIU checkpoint directory.
			rootfs := filepath.Join(dir, e.Name(), "rootfs.ext4")
			if info, err := os.Stat(rootfs); err == nil {
				sizeMB := float64(info.Size()) / 1024 / 1024
				checkpoints = append(checkpoints, cpEntry{
					name:   e.Name(),
					size:   fmt.Sprintf("%.1f MB", sizeMB),
					cpType: "criu",
				})
			}
		} else if filepath.Ext(e.Name()) == ".ext4" {
			// Disk-only checkpoint.
			info, err := e.Info()
			if err != nil {
				continue
			}
			sizeMB := float64(info.Size()) / 1024 / 1024
			checkpoints = append(checkpoints, cpEntry{
				name:   e.Name(),
				size:   fmt.Sprintf("%.1f MB", sizeMB),
				cpType: "disk",
			})
		}
	}

	sort.Slice(checkpoints, func(i, j int) bool {
		return checkpoints[i].name < checkpoints[j].name
	})

	if len(checkpoints) == 0 {
		fmt.Println("no checkpoints")
		return nil
	}

	t := newTable("NAME", "TYPE", "SIZE")
	for _, cp := range checkpoints {
		t.Row(cp.name, cp.cpType, cp.size)
	}
	fmt.Println(t)
	return nil
}
