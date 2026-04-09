package lnx

import (
	"fmt"
	"io"
	"os"
	"path/filepath"
)

type machineSnapshotRuntime struct {
	vm          SnapshotCapableVirtualMachine
	kernelPath  string
	initrdPath  string
	rootfsPath  string
	swapPath    string
	commandLine string
	hostname    string
	user        string
	homeDir     string
	cwd         string
	shares      []string
	sshAgent    bool
	cpus        uint
	memoryBytes uint64
}

func (m *machineSnapshotRuntime) createClone(destDir string, syncGuest func() error) error {
	if m == nil || m.vm == nil {
		return fmt.Errorf("machine snapshot unavailable")
	}
	if err := m.vm.ValidateSaveRestoreSupport(); err != nil {
		return fmt.Errorf("vm save/restore unsupported: %w", err)
	}
	if err := os.MkdirAll(destDir, 0755); err != nil {
		return fmt.Errorf("create clone dir: %w", err)
	}
	if syncGuest != nil {
		if err := syncGuest(); err != nil {
			return err
		}
	}
	if err := m.vm.Pause(); err != nil {
		return fmt.Errorf("pause vm: %w", err)
	}
	resumeNeeded := true
	defer func() {
		if resumeNeeded {
			_ = m.vm.Resume()
		}
	}()

	if err := cloneFile(m.rootfsPath, filepath.Join(destDir, "rootfs.ext4")); err != nil {
		return fmt.Errorf("clone rootfs: %w", err)
	}
	if m.swapPath != "" {
		if err := cloneFile(m.swapPath, filepath.Join(destDir, "swap.img")); err != nil {
			return fmt.Errorf("clone swap: %w", err)
		}
	}

	snapDir := MachineSnapshotDir(destDir)
	if err := os.RemoveAll(snapDir); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("reset machine snapshot dir: %w", err)
	}
	if err := os.MkdirAll(snapDir, 0755); err != nil {
		return fmt.Errorf("create machine snapshot dir: %w", err)
	}

	kernelDst := filepath.Join(snapDir, "kernel")
	if err := copyRegularFile(kernelDst, m.kernelPath); err != nil {
		return fmt.Errorf("copy kernel: %w", err)
	}
	initrdDst := filepath.Join(snapDir, "initramfs.cpio")
	if err := copyRegularFile(initrdDst, m.initrdPath); err != nil {
		return fmt.Errorf("copy initramfs: %w", err)
	}

	stateDst := filepath.Join(snapDir, "machine-state.vzvmsave")
	if err := m.vm.SaveMachineStateToPath(stateDst); err != nil {
		return fmt.Errorf("save machine state: %w", err)
	}

	manifest := MachineSnapshotManifest{
		Version:     1,
		KernelPath:  filepath.Base(kernelDst),
		InitrdPath:  filepath.Base(initrdDst),
		CommandLine: m.commandLine,
		StatePath:   filepath.Base(stateDst),
		RootfsPath:  filepath.Join("..", "rootfs.ext4"),
		SwapPath:    filepath.Join("..", "swap.img"),
		Hostname:    m.hostname,
		User:        m.user,
		HomeDir:     m.homeDir,
		CWD:         m.cwd,
		Shares:      append([]string(nil), m.shares...),
		SSHAgent:    m.sshAgent,
		CPUs:        m.cpus,
		MemoryBytes: m.memoryBytes,
	}
	if m.swapPath == "" {
		manifest.SwapPath = ""
	}
	if err := WriteMachineSnapshotManifest(snapDir, manifest); err != nil {
		return err
	}

	if err := m.vm.Resume(); err != nil {
		return fmt.Errorf("resume vm: %w", err)
	}
	resumeNeeded = false
	return nil
}

func copyRegularFile(dst, src string) error {
	if err := cloneFile(src, dst); err == nil {
		return nil
	}

	in, err := os.Open(src)
	if err != nil {
		return err
	}
	defer in.Close()

	out, err := os.Create(dst)
	if err != nil {
		return err
	}
	defer out.Close()

	if _, err := io.Copy(out, in); err != nil {
		return err
	}
	return out.Close()
}
