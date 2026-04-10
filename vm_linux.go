//go:build linux

package lnx

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

// firecrackerVM implements VirtualMachine by managing a Firecracker process
// configured via its REST API.
type firecrackerVM struct {
	cmd            *exec.Cmd
	apiSock        string
	vsock          *firecrackerVsock
	sockDir        string
	loadedSnapshot bool

	stateCh chan VMState
	once    sync.Once
	done    chan struct{} // closed when the process exits
}

// buildVM creates and configures a Firecracker VM ready to start.
// Requires LNX_EXPERIMENTS=linux_host (experimental feature).
func buildVM(cfg *Config, initrdPath, cwd, swapPath, homeDir string) (VirtualMachine, error) {
	if !linuxHostEnabled() {
		return nil, fmt.Errorf("Linux host support is experimental; set LNX_EXPERIMENTS=linux_host to enable")
	}
	origSocketDir := cfg.socketDir()
	slog.Info("firecracker buildVM starting", "kernel", cfg.KernelPath, "rootfs", cfg.RootfsPath, "socket_dir", origSocketDir, "restore", cfg.MemorySnapshot != nil)

	// Firecracker sockets must be on a local filesystem (9P doesn't support
	// Unix domain sockets). Override socketDir to /var/run/lnx/<instance>.
	sockDir := filepath.Join("/var/run/lnx", filepath.Base(cfg.socketDir()))
	os.MkdirAll(sockDir, 0755)
	cfg.SocketDir = sockDir

	apiSock := filepath.Join(sockDir, "firecracker.sock")
	vsockPath := filepath.Join(sockDir, "vsock")

	// Clean up stale sockets.
	os.Remove(apiSock)
	os.Remove(vsockPath)

	// Set up TAP networking (requires root/CAP_NET_ADMIN).
	slog.Info("firecracker configuring TAP networking")
	if err := setupTAP(); err != nil {
		return nil, fmt.Errorf("setup TAP: %w", err)
	}

	// Set up serial console log file.
	serialPath := firecrackerSerialLogPath(cfg, origSocketDir, sockDir)
	serialFile, err := os.Create(serialPath)
	if err != nil {
		return nil, fmt.Errorf("create serial log: %w", err)
	}

	fcBin := findFirecracker()
	cmd := exec.Command(fcBin, "--api-sock", apiSock)
	cmd.Stderr = serialFile
	cmd.Stdout = serialFile

	if err := cmd.Start(); err != nil {
		serialFile.Close()
		return nil, fmt.Errorf("start firecracker: %w", err)
	}
	serialFile.Close()
	slog.Info("firecracker process started", "api_sock", apiSock, "pid", cmd.Process.Pid)

	vm := &firecrackerVM{
		cmd:     cmd,
		apiSock: apiSock,
		vsock:   newFirecrackerVsock(vsockPath),
		sockDir: sockDir,
		stateCh: make(chan VMState, 8),
		done:    make(chan struct{}),
	}

	// Monitor process exit.
	go func() {
		cmd.Wait()
		vm.stateCh <- VMStateStopped
		close(vm.done)
	}()

	// Wait for the API socket to appear.
	if err := waitForSocket(apiSock, 5*time.Second); err != nil {
		cmd.Process.Kill()
		return nil, fmt.Errorf("firecracker API socket: %w", err)
	}
	slog.Info("firecracker API socket ready", "api_sock", apiSock)

	// Configure the VM via the Firecracker REST API.
	if cfg.MemorySnapshot != nil {
		vm.loadedSnapshot = true
		slog.Info("firecracker loading snapshot", "state_path", cfg.MemorySnapshot.StatePath, "mem_path", cfg.MemorySnapshot.MemPath)
		if err := vm.loadSnapshot(cfg.MemorySnapshot); err != nil {
			cmd.Process.Kill()
			return nil, fmt.Errorf("load firecracker snapshot: %w", err)
		}
	} else {
		slog.Info("firecracker configuring fresh VM")
		if err := vm.configure(cfg, initrdPath, cwd, swapPath, homeDir, vsockPath); err != nil {
			cmd.Process.Kill()
			return nil, fmt.Errorf("configure firecracker: %w", err)
		}
	}
	slog.Info("firecracker buildVM complete", "api_sock", apiSock)

	return vm, nil
}

func firecrackerSerialLogPath(cfg *Config, origSocketDir, sockDir string) string {
	if cfg.InstanceDir != "" && strings.Contains(origSocketDir, "/memorysnapshot/inner") {
		dir := filepath.Join(cfg.InstanceDir, "memorysnapshot")
		if err := os.MkdirAll(dir, 0755); err == nil {
			return filepath.Join(dir, "inner-serial.log")
		}
	}
	return filepath.Join(sockDir, "serial.log")
}

func (vm *firecrackerVM) configure(cfg *Config, initrdPath, cwd, swapPath, homeDir, vsockPath string) error {
	cmdline := fmt.Sprintf("console=ttyS0 lnx.epoch=%d reboot=k panic=1", time.Now().Unix())

	// 1. Boot source.
	if err := vm.apiPut("/boot-source", map[string]any{
		"kernel_image_path": cfg.KernelPath,
		"initrd_path":       initrdPath,
		"boot_args":         cmdline,
	}); err != nil {
		return fmt.Errorf("boot source: %w", err)
	}

	// 2. Machine config.
	if err := vm.apiPut("/machine-config", map[string]any{
		"vcpu_count":   cfg.cpus(),
		"mem_size_mib": cfg.memoryBytes() / (1024 * 1024),
	}); err != nil {
		return fmt.Errorf("machine config: %w", err)
	}

	// 3. Root drive.
	if err := vm.apiPut("/drives/rootfs", map[string]any{
		"drive_id":       "rootfs",
		"path_on_host":   cfg.RootfsPath,
		"is_root_device": true,
		"is_read_only":   false,
	}); err != nil {
		return fmt.Errorf("root drive: %w", err)
	}

	// 4. Swap drive.
	if err := vm.apiPut("/drives/swap", map[string]any{
		"drive_id":       "swap",
		"path_on_host":   swapPath,
		"is_root_device": false,
		"is_read_only":   false,
	}); err != nil {
		return fmt.Errorf("swap drive: %w", err)
	}

	// 5. Nested instance rootfs drives.
	for i, nr := range cfg.NestedRootfs {
		driveID := fmt.Sprintf("nested%d", i)
		if err := vm.apiPut("/drives/"+driveID, map[string]any{
			"drive_id":       driveID,
			"path_on_host":   nr.RootfsPath,
			"is_root_device": false,
			"is_read_only":   false,
		}); err != nil {
			return fmt.Errorf("nested drive %s: %w", nr.InstanceName, err)
		}
	}

	// 6. Network interface.
	if err := vm.apiPut("/network-interfaces/eth0", map[string]any{
		"iface_id":      "eth0",
		"host_dev_name": "lnxtap0",
		"guest_mac":     "06:00:AC:10:00:02",
	}); err != nil {
		return fmt.Errorf("network interface: %w", err)
	}

	// 7. Vsock device.
	if err := vm.apiPut("/vsock", map[string]any{
		"guest_cid": 3,
		"uds_path":  vsockPath,
	}); err != nil {
		return fmt.Errorf("vsock: %w", err)
	}

	return nil
}

func (vm *firecrackerVM) Start() error {
	if vm.loadedSnapshot {
		if err := vm.Resume(); err != nil {
			return fmt.Errorf("resume instance: %w", err)
		}
		vm.stateCh <- VMStateRunning
		return nil
	}
	if err := vm.apiPut("/actions", map[string]any{
		"action_type": "InstanceStart",
	}); err != nil {
		return fmt.Errorf("start instance: %w", err)
	}
	vm.stateCh <- VMStateRunning
	return nil
}

func (vm *firecrackerVM) Stop() error {
	vm.once.Do(func() {
		// Try graceful shutdown via API first.
		err := vm.apiPut("/actions", map[string]any{
			"action_type": "SendCtrlAltDel",
		})
		if err != nil {
			slog.Debug("SendCtrlAltDel failed, killing process", "error", err)
		}

		// Give it a moment, then force kill.
		select {
		case <-vm.done:
			return
		case <-time.After(3 * time.Second):
			vm.cmd.Process.Kill()
		}
	})

	<-vm.done
	vm.vsock.cleanup()
	teardownTAP()
	os.Remove(vm.apiSock)
	return nil
}

func (vm *firecrackerVM) RequestStop() error {
	return vm.apiPut("/actions", map[string]any{
		"action_type": "SendCtrlAltDel",
	})
}

func (vm *firecrackerVM) Pause() error {
	return vm.apiPatch("/vm", map[string]any{"state": "Paused"})
}

func (vm *firecrackerVM) Resume() error {
	return vm.apiPatch("/vm", map[string]any{"state": "Resumed"})
}

func (vm *firecrackerVM) CreateMemorySnapshot(statePath, memPath string) error {
	return vm.apiPut("/snapshot/create", map[string]any{
		"snapshot_type": "Full",
		"snapshot_path": statePath,
		"mem_file_path": memPath,
	})
}

func (vm *firecrackerVM) loadSnapshot(snapshot *MemorySnapshot) error {
	return vm.apiPut("/snapshot/load", map[string]any{
		"snapshot_path": snapshot.StatePath,
		"mem_backend": map[string]any{
			"backend_type": "File",
			"backend_path": snapshot.MemPath,
		},
		"resume_vm": false,
	})
}

func (vm *firecrackerVM) StateChangedNotify() <-chan VMState {
	return vm.stateCh
}

func (vm *firecrackerVM) VsockDevice() VsockDevice {
	return vm.vsock
}

// shutdownVM gracefully shuts down a Firecracker VM.
func shutdownVM(vm VirtualMachine, exitCode int) {
	if exitCode == 130 {
		vm.Stop()
		return
	}

	vm.RequestStop()
	select {
	case <-time.After(3 * time.Second):
		vm.Stop()
	case state := <-vm.StateChangedNotify():
		if state != VMStateStopped {
			vm.Stop()
		}
	}
}

func (vm *firecrackerVM) apiPut(path string, body any) error {
	return vm.apiRequest(http.MethodPut, path, body)
}

func (vm *firecrackerVM) apiPatch(path string, body any) error {
	return vm.apiRequest(http.MethodPatch, path, body)
}

func (vm *firecrackerVM) apiRequest(method, path string, body any) error {
	data, err := json.Marshal(body)
	if err != nil {
		return err
	}

	client := &http.Client{
		Transport: &http.Transport{
			DialContext: func(_ context.Context, _, _ string) (net.Conn, error) {
				return net.Dial("unix", vm.apiSock)
			},
		},
	}

	req, err := http.NewRequest(method, "http://localhost"+path, bytes.NewReader(data))
	if err != nil {
		return err
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := client.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()

	if resp.StatusCode >= 300 {
		var apiErr struct {
			FaultMessage string `json:"fault_message"`
		}
		json.NewDecoder(resp.Body).Decode(&apiErr)
		return fmt.Errorf("HTTP %d: %s", resp.StatusCode, apiErr.FaultMessage)
	}
	return nil
}

func linuxHostEnabled() bool { return ExperimentEnabled("linux_host") }

// findFirecracker returns the path to the firecracker binary.
// Checks PATH first, then ~/.lnx/bin/. If found on a 9P mount (which
// doesn't support mmap/exec), copies to a local cache first.
func findFirecracker() string {
	// Prefer a binary already on local storage.
	if path, err := exec.LookPath("firecracker"); err == nil {
		return path
	}

	home, err := os.UserHomeDir()
	if err != nil {
		return "firecracker"
	}

	lnxBin := filepath.Join(home, ".lnx", "bin", "firecracker")
	if _, err := os.Stat(lnxBin); err != nil {
		return "firecracker"
	}

	// The binary might be on a 9P mount which doesn't support exec.
	// Copy to local storage (/var/cache/lnx, always on ext4) if needed.
	localCache := "/var/cache/lnx"
	os.MkdirAll(localCache, 0755)
	localBin := filepath.Join(localCache, "firecracker")

	// Check if cached copy exists and matches size.
	srcInfo, _ := os.Stat(lnxBin)
	dstInfo, dstErr := os.Stat(localBin)
	if dstErr == nil && dstInfo.Size() == srcInfo.Size() {
		return localBin
	}

	// Copy to local cache.
	src, err := os.Open(lnxBin)
	if err != nil {
		return lnxBin // best effort
	}
	defer src.Close()

	dst, err := os.OpenFile(localBin, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0755)
	if err != nil {
		return lnxBin
	}
	defer dst.Close()

	if _, err := io.Copy(dst, src); err != nil {
		os.Remove(localBin)
		return lnxBin
	}

	slog.Debug("cached firecracker binary locally", "src", lnxBin, "dst", localBin)
	return localBin
}

// waitForSocket polls for a Unix socket to appear.
func waitForSocket(path string, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		conn, err := net.Dial("unix", path)
		if err == nil {
			conn.Close()
			return nil
		}
		time.Sleep(10 * time.Millisecond)
	}
	return fmt.Errorf("socket %s did not appear within %v", path, timeout)
}
