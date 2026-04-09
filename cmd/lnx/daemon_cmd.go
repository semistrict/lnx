package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"

	"github.com/semistrict/lnx"
	"github.com/semistrict/lnx/internal/protocol"
	"github.com/spf13/cobra"
)

var daemonCmd = &cobra.Command{
	Use:    "_daemon",
	Short:  "Run VM as a background daemon (internal use)",
	Hidden: true,
	RunE: func(cmd *cobra.Command, args []string) error {
		lnx.InitBinary = initBinary
		dir := instanceDir()

		rootfsPath, socketDir := resolveRootfs(dir)
		restore, err := loadMachineRestore(dir)
		if err != nil {
			return err
		}

		// Scan for nested instances to attach as block devices.
		nested := scanNestedInstances()

		cfg := &lnx.Config{
			KernelPath:   resolveKernel(),
			RootfsPath:   rootfsPath,
			Hostname:     qualifiedInstanceName() + ".lnx",
			Checkpoint:   doCheckpoint,
			Ephemeral:    doEphemeral,
			SSHAgent:     doSSHAgent,
			Shares:       loadShares(dir),
			SocketDir:    socketDir,
			NestedRootfs: nested,
			Restore:      restore,
		}
		if restore != nil {
			cfg.KernelPath = restore.Manifest.KernelPath
			cfg.InitramfsPath = restore.Manifest.InitrdPath
			cfg.CommandLine = restore.Manifest.CommandLine
			cfg.RootfsPath = restore.Manifest.RootfsPath
			cfg.Hostname = restore.Manifest.Hostname
			cfg.SSHAgent = restore.Manifest.SSHAgent
			cfg.Shares = append([]string(nil), restore.Manifest.Shares...)
			cfg.CPUs = restore.Manifest.CPUs
			cfg.MemoryBytes = restore.Manifest.MemoryBytes
		}

		err = lnx.RunDaemon(cfg)
		if err != nil {
			errPath := filepath.Join(socketDir, "error.log")
			os.WriteFile(errPath, []byte(err.Error()+"\n"), 0644)
			return err
		}
		os.Remove(filepath.Join(socketDir, "error.log"))
		return nil
	},
}

func init() {
	daemonCmd.Flags().BoolVarP(&doCheckpoint, "checkpoint", "c", false, "snapshot rootfs before starting")
	daemonCmd.Flags().BoolVar(&doEphemeral, "ephemeral", false, "clone rootfs to a temp file; discard on exit")
	daemonCmd.Flags().BoolVar(&doSSHAgent, "ssh-agent", false, "forward host SSH agent into the guest")
	rootCmd.AddCommand(daemonCmd)
}

// resolveRootfs returns the rootfs path and socket directory for the current instance.
// For nested instances, the rootfs is a block device (/dev/vdX) discovered
// from the drives mapping written by the parent's guest init.
func resolveRootfs(instanceDir string) (rootfsPath, socketDir string) {
	// Check for a nested drive mapping first (block device takes priority).
	qname := qualifiedInstanceName()
	if dev := lookupNestedDrive(qname); dev != "" {
		workDir := filepath.Join("/var/lib/lnx/instances", qname)
		os.MkdirAll(workDir, 0755)
		return dev, workDir
	}

	// Normal case: rootfs is a file in the instance directory.
	return filepath.Join(instanceDir, "rootfs.ext4"), instanceDir
}

func loadMachineRestore(instanceDir string) (*lnx.MachineRestore, error) {
	if !experimentEnabled("memorysnapshot") {
		return nil, nil
	}
	return lnx.LoadMachineRestore(instanceDir)
}

const nestedDrivesPath = "/var/lib/lnx/nested-drives.json"

// lookupNestedDrive reads the drives mapping and returns the device path
// for the given instance name, or empty if not found.
func lookupNestedDrive(instanceName string) string {
	data, err := os.ReadFile(nestedDrivesPath)
	if err != nil {
		return ""
	}
	var drives []protocol.NestedDrive
	if err := json.Unmarshal(data, &drives); err != nil {
		return ""
	}
	for _, d := range drives {
		if d.InstanceName == instanceName {
			return d.DevicePath
		}
	}
	return ""
}

// scanNestedInstances finds rootfs files for nested instances of the current
// instance. For instance "default", it looks for "default.*" directories.
func scanNestedInstances() []lnx.NestedRootfs {
	parent := qualifiedInstanceName()
	prefix := parent + "."
	instancesDir := filepath.Join(lnxBase(), "instances")

	entries, err := os.ReadDir(instancesDir)
	if err != nil {
		return nil
	}

	var nested []lnx.NestedRootfs
	for _, e := range entries {
		if !e.IsDir() || !strings.HasPrefix(e.Name(), prefix) {
			continue
		}
		rootfs := filepath.Join(instancesDir, e.Name(), "rootfs.ext4")
		if _, err := os.Stat(rootfs); err != nil {
			continue
		}
		nested = append(nested, lnx.NestedRootfs{
			InstanceName: e.Name(),
			RootfsPath:   rootfs,
		})
	}
	return nested
}
