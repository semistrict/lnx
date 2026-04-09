package lnx

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
)

const machineSnapshotDirName = "machine-snapshot"

type MachineSnapshotManifest struct {
	Version     int      `json:"version"`
	KernelPath  string   `json:"kernel_path"`
	InitrdPath  string   `json:"initrd_path"`
	CommandLine string   `json:"command_line"`
	StatePath   string   `json:"state_path"`
	RootfsPath  string   `json:"rootfs_path"`
	SwapPath    string   `json:"swap_path"`
	Hostname    string   `json:"hostname"`
	User        string   `json:"user"`
	HomeDir     string   `json:"home_dir"`
	CWD         string   `json:"cwd"`
	Shares      []string `json:"shares,omitempty"`
	SSHAgent    bool     `json:"ssh_agent,omitempty"`
	CPUs        uint     `json:"cpus"`
	MemoryBytes uint64   `json:"memory_bytes"`
}

type MachineRestore struct {
	Dir      string
	Manifest MachineSnapshotManifest
}

func MachineSnapshotDir(instanceDir string) string {
	return filepath.Join(instanceDir, machineSnapshotDirName)
}

func WriteMachineSnapshotManifest(dir string, manifest MachineSnapshotManifest) error {
	if err := os.MkdirAll(dir, 0755); err != nil {
		return fmt.Errorf("create machine snapshot dir: %w", err)
	}
	data, err := json.MarshalIndent(manifest, "", "  ")
	if err != nil {
		return fmt.Errorf("marshal machine snapshot manifest: %w", err)
	}
	if err := os.WriteFile(filepath.Join(dir, "manifest.json"), data, 0644); err != nil {
		return fmt.Errorf("write machine snapshot manifest: %w", err)
	}
	return nil
}

func LoadMachineRestore(instanceDir string) (*MachineRestore, error) {
	dir := MachineSnapshotDir(instanceDir)
	data, err := os.ReadFile(filepath.Join(dir, "manifest.json"))
	if err != nil {
		if os.IsNotExist(err) {
			return nil, nil
		}
		return nil, fmt.Errorf("read machine snapshot manifest: %w", err)
	}

	var manifest MachineSnapshotManifest
	if err := json.Unmarshal(data, &manifest); err != nil {
		return nil, fmt.Errorf("decode machine snapshot manifest: %w", err)
	}
	if manifest.Version != 1 {
		return nil, fmt.Errorf("unsupported machine snapshot manifest version %d", manifest.Version)
	}

	resolve := func(p string) string {
		if p == "" || filepath.IsAbs(p) {
			return p
		}
		return filepath.Join(dir, p)
	}
	manifest.KernelPath = resolve(manifest.KernelPath)
	manifest.InitrdPath = resolve(manifest.InitrdPath)
	manifest.StatePath = resolve(manifest.StatePath)
	manifest.RootfsPath = resolve(manifest.RootfsPath)
	manifest.SwapPath = resolve(manifest.SwapPath)

	return &MachineRestore{Dir: dir, Manifest: manifest}, nil
}

func RemoveMachineSnapshot(instanceDir string) error {
	if err := os.RemoveAll(MachineSnapshotDir(instanceDir)); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("remove machine snapshot: %w", err)
	}
	return nil
}
