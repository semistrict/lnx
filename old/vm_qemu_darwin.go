//go:build darwin

package lnx

import (
	"encoding/json"
	"fmt"
	"log/slog"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/semistrict/lnx/internal/lnxnet"
)

// qemuVM implements VirtualMachine by managing a QEMU process.
type qemuVM struct {
	cmd      *exec.Cmd
	vsock    *qemuVsock
	bridge   *lnxnet.Bridge
	sockDir  string
	qmpSock  string // QMP Unix socket path for control commands
	logFile  *os.File
	ramClone string // CoW clone of ram.img to remove on shutdown (empty if base)
	restored bool   // VM was restored from migration snapshot (fork)

	// ramPath is the active RAM memory-backend-file path.
	// May be ram-EPOCH.img (a CoW clone of the base ram.img).
	ramPath string

	stateCh chan VMState
	once    sync.Once
	done    chan struct{}
}

func (q *qemuVM) Start() error {
	if q.restored {
		// VM is already running after migration restore; cont is unnecessary.
		q.stateCh <- VMStateRunning
		return nil
	}
	// QEMU was started with -S (paused). Resume via QMP.
	if err := qmpCommand(q.qmpSock, "cont"); err != nil {
		return fmt.Errorf("qemu resume: %w", err)
	}
	q.stateCh <- VMStateRunning
	return nil
}

// IsRestored reports whether this VM was restored from a migration snapshot.
func (q *qemuVM) IsRestored() bool {
	return q.restored
}

func (q *qemuVM) Stop() error {
	q.once.Do(func() {
		q.cmd.Process.Signal(syscall.SIGTERM)
		select {
		case <-q.done:
			return
		case <-time.After(3 * time.Second):
			q.cmd.Process.Kill()
		}
	})
	<-q.done
	q.vsock.cleanup()
	if q.bridge != nil {
		q.bridge.Close()
	}
	os.Remove(q.qmpSock)
	if q.ramClone != "" {
		os.Remove(q.ramClone)
	}
	if q.logFile != nil {
		q.logFile.Close()
	}
	return nil
}

func (q *qemuVM) RequestStop() error {
	return q.cmd.Process.Signal(syscall.SIGTERM)
}

func (q *qemuVM) StateChangedNotify() <-chan VMState {
	return q.stateCh
}

// QMPSock returns the path to the QMP Unix socket.
func (q *qemuVM) QMPSock() string { return q.qmpSock }

// RamPath returns the active RAM memory-backend-file path.
// May be ram-EPOCH.img (a CoW clone of the base ram.img).
func (q *qemuVM) RamPath() string { return q.ramPath }

// Resume sends a QMP cont command to unpause the VM.
func (q *qemuVM) Resume() error { return qmpCommand(q.qmpSock, "cont") }

// QMPResume sends a cont command to the QEMU VM at the given QMP socket.
// Exported for integration tests that call ForkQemuVM directly.
func QMPResume(qmpSock string) error { return qmpCommand(qmpSock, "cont") }

func (q *qemuVM) VsockDevice() VsockDevice {
	return q.vsock
}

// ParseQemuBackend returns the QEMU binary path from LNX_BACKEND,
// or empty string if LNX_BACKEND is not set to qemu.
func ParseQemuBackend() string {
	return parseQemuBackend()
}

func parseQemuBackend() string {
	val := os.Getenv("LNX_BACKEND")
	if val == "" {
		return ""
	}
	if val == "qemu" {
		if p, err := exec.LookPath("qemu-system-aarch64"); err == nil {
			return p
		}
		return "qemu-system-aarch64"
	}
	if after, ok := strings.CutPrefix(val, "qemu:"); ok {
		return after
	}
	return ""
}

func buildQemuVM(cfg *Config, qemuBin, initrdPath, swapPath, criuPath, macAddr string, epoch int64) (VirtualMachine, error) {
	sockDir := cfg.socketDir()

	vsockPath := filepath.Join(sockDir, "qemu-vsock")
	serialPath := filepath.Join(sockDir, "serial.log")
	qmpPath := filepath.Join(sockDir, "qmp.sock")

	os.Remove(vsockPath)
	os.Remove(qmpPath)

	cmdline := fmt.Sprintf("console=ttyAMA0 lnx.epoch=%d", epoch)

	memMB := cfg.memoryBytes() / (1024 * 1024)

	// RAM file lives alongside rootfs so ephemeral mode's cleanup covers it.
	// If a ram.img from a previous boot exists, clonefile it (CoW) so this
	// boot starts with a private copy that shares pages until written.
	rootfsDir := filepath.Dir(cfg.RootfsPath)
	baseRAM := filepath.Join(rootfsDir, "ram.img")
	ramPath := baseRAM
	if _, err := os.Stat(baseRAM); err == nil {
		ramPath = filepath.Join(rootfsDir, fmt.Sprintf("ram-%d.img", epoch))
		if err := cloneFile(baseRAM, ramPath); err != nil {
			return nil, fmt.Errorf("clone ram: %w", err)
		}
		slog.Debug("cloned ram", "src", baseRAM, "dst", ramPath)
	}

	args := []string{
		"-machine", "virt,accel=hvf,memory-backend=mem0",
		"-cpu", "host",
		"-object", fmt.Sprintf("memory-backend-file,id=mem0,size=%dM,mem-path=%s,share=on", memMB, ramPath),
		"-smp", fmt.Sprintf("%d", cfg.cpus()),
		"-kernel", cfg.KernelPath,
		"-initrd", initrdPath,
		"-append", cmdline,
		// Rootfs, swap, criu — same vda/vdb/vdc order as vz backend.
		"-drive", fmt.Sprintf("file=%s,format=raw,if=virtio", cfg.RootfsPath),
		"-drive", fmt.Sprintf("file=%s,format=raw,if=virtio", swapPath),
		"-drive", fmt.Sprintf("file=%s,format=raw,if=virtio", criuPath),
	}

	// Nested rootfs drives (vdd, vde, ...).
	for _, nr := range cfg.NestedRootfs {
		args = append(args,
			"-drive", fmt.Sprintf("file=%s,format=raw,if=virtio", nr.RootfsPath),
		)
	}

	// Vsock.
	args = append(args,
		"-device", fmt.Sprintf("virtio-vsock-pci,guest-cid=3,socket-path=%s", vsockPath),
	)

	// Userspace networking via socketpair + lnxnet.Bridge.
	// Create a Unix datagram socketpair: hostFd for the bridge, vmFd for QEMU.
	netFds, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_DGRAM, 0)
	if err != nil {
		return nil, fmt.Errorf("net socketpair: %w", err)
	}
	for _, fd := range netFds {
		syscall.SetsockoptInt(fd, syscall.SOL_SOCKET, syscall.SO_SNDBUF, 1*1024*1024)
		syscall.SetsockoptInt(fd, syscall.SOL_SOCKET, syscall.SO_RCVBUF, 4*1024*1024)
	}
	hostNetFd, vmNetFd := netFds[0], netFds[1]

	// vmNetFd is passed to QEMU via ExtraFiles. ExtraFiles[0] = fd 3 in child.
	vmNetFile := os.NewFile(uintptr(vmNetFd), "qemu-net")
	// QEMU's -netdev socket,fd=3 connects virtio-net to this datagram socket.
	netDev := "virtio-net-pci,netdev=net0"
	if macAddr != "" {
		netDev += ",mac=" + macAddr
	}
	args = append(args,
		"-netdev", "socket,id=net0,fd=3",
		"-device", netDev,
	)

	// Serial console, QMP monitor, display.
	// Check for incoming migration state (VM fork).
	incomingFile := filepath.Join(rootfsDir, "incoming.bin")
	isRestore := false
	if _, err := os.Stat(incomingFile); err == nil {
		isRestore = true
	}

	args = append(args,
		"-serial", "file:"+serialPath,
		"-qmp", fmt.Sprintf("unix:%s,server=on,wait=off", qmpPath),
		"-monitor", "none",
		"-nographic",
		"-no-reboot",
	)
	if isRestore {
		args = append(args, "-incoming", "defer")
	} else {
		args = append(args, "-S") // Start paused; resumed by Start() after vsock listeners are ready.
	}

	slog.Debug("starting qemu", "bin", qemuBin, "args", args)

	qemuLogPath := filepath.Join(sockDir, "qemu.log")
	qemuLog, err := os.OpenFile(qemuLogPath, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0644)
	if err != nil {
		syscall.Close(hostNetFd)
		vmNetFile.Close()
		return nil, fmt.Errorf("create qemu log: %w", err)
	}

	bridge := lnxnet.NewBridgeFromFd(hostNetFd)

	cmd := exec.Command(qemuBin, args...)
	cmd.Stdout = qemuLog
	cmd.Stderr = qemuLog
	cmd.ExtraFiles = []*os.File{vmNetFile} // fd 3 in child

	if err := cmd.Start(); err != nil {
		qemuLog.Close()
		bridge.Close()
		vmNetFile.Close()
		return nil, fmt.Errorf("start qemu: %w", err)
	}
	// Child has its own copy of vmNetFd via ExtraFiles; close the parent's.
	vmNetFile.Close()
	// Start the userspace network bridge (ARP, DHCP, TCP/UDP relay).
	bridge.Start()

	var ramClonePath string
	if ramPath != baseRAM {
		ramClonePath = ramPath
	}

	vm := &qemuVM{
		cmd:      cmd,
		vsock:    newQemuVsock(vsockPath),
		bridge:   bridge,
		sockDir:  sockDir,
		qmpSock:  qmpPath,
		logFile:  qemuLog,
		ramClone: ramClonePath,
		restored: isRestore,
		ramPath:  ramPath,
		stateCh:  make(chan VMState, 8),
		done:     make(chan struct{}),
	}

	go func() {
		cmd.Wait()
		vm.stateCh <- VMStateStopped
		close(vm.done)
	}()

	// Wait for QMP socket and negotiate capabilities.
	if err := qmpWaitAndHandshake(qmpPath, 5*time.Second); err != nil {
		cmd.Process.Kill()
		return nil, fmt.Errorf("qemu qmp: %w\n%s", err, qemuLogTail(sockDir))
	}

	// For restore: set CPR-reboot mode + x-ignore-shared, then load state.
	if isRestore {
		if err := qmpRestore(qmpPath, incomingFile); err != nil {
			cmd.Process.Kill()
			return nil, fmt.Errorf("qemu restore: %w\n%s", err, qemuLogTail(sockDir))
		}
		os.Remove(incomingFile) // consumed; prevent re-restore on future boots
	}

	return vm, nil
}

func qmpRestore(sockPath, stateFile string) error {
	conn, err := net.Dial("unix", sockPath)
	if err != nil {
		return err
	}
	defer conn.Close()
	conn.SetDeadline(time.Now().Add(30 * time.Second))

	buf := make([]byte, 8192)
	conn.Read(buf) // greeting
	conn.Write([]byte(`{"execute":"qmp_capabilities"}` + "\n"))
	conn.Read(buf)

	// Set CPR-reboot mode and x-ignore-shared.
	conn.Write([]byte(`{"execute":"migrate-set-parameters","arguments":{"mode":"cpr-reboot"}}` + "\n"))
	time.Sleep(100 * time.Millisecond)
	conn.Read(buf)

	conn.Write([]byte(`{"execute":"migrate-set-capabilities","arguments":{"capabilities":[{"capability":"x-ignore-shared","state":true}]}}` + "\n"))
	time.Sleep(100 * time.Millisecond)
	conn.Read(buf)

	// Trigger incoming migration.
	migrateCmd, _ := json.Marshal(map[string]any{
		"execute":   "migrate-incoming",
		"arguments": map[string]string{"uri": "file:" + stateFile},
	})
	conn.Write(append(migrateCmd, '\n'))

	// Wait for restore to complete.
	for i := 0; i < 60; i++ {
		time.Sleep(500 * time.Millisecond)
		conn.Write([]byte(`{"execute":"query-migrate"}` + "\n"))
		n, err := conn.Read(buf)
		if err != nil {
			return fmt.Errorf("query-migrate: %w", err)
		}
		resp := string(buf[:n])
		if strings.Contains(resp, `"completed"`) {
			// Resume the VM — it's paused after migration.
			conn.Write([]byte(`{"execute":"cont"}` + "\n"))
			conn.Read(buf)
			return nil
		}
		if strings.Contains(resp, `"failed"`) {
			return fmt.Errorf("restore failed: %s", resp)
		}
	}
	return fmt.Errorf("restore timed out")
}

// waitForUnixSocket polls until a Unix socket appears and is connectable.
func waitForUnixSocket(path string, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		conn, err := net.Dial("unix", path)
		if err == nil {
			conn.Close()
			return nil
		}
		time.Sleep(50 * time.Millisecond)
	}
	return fmt.Errorf("socket %s did not appear within %v", path, timeout)
}

// qmpWaitAndHandshake waits for the QMP socket to appear, connects, reads
// the greeting, and sends qmp_capabilities to enter command mode.
func qmpWaitAndHandshake(sockPath string, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	var conn net.Conn
	var err error
	for time.Now().Before(deadline) {
		conn, err = net.Dial("unix", sockPath)
		if err == nil {
			break
		}
		time.Sleep(50 * time.Millisecond)
	}
	if err != nil {
		return fmt.Errorf("connect %s: %w", sockPath, err)
	}
	defer conn.Close()

	conn.SetDeadline(deadline)

	// Read greeting.
	buf := make([]byte, 4096)
	n, err := conn.Read(buf)
	if err != nil {
		return fmt.Errorf("read greeting: %w", err)
	}
	slog.Debug("qmp greeting", "data", string(buf[:n]))

	// Send qmp_capabilities.
	if _, err := conn.Write([]byte(`{"execute":"qmp_capabilities"}` + "\n")); err != nil {
		return fmt.Errorf("send qmp_capabilities: %w", err)
	}
	n, err = conn.Read(buf)
	if err != nil {
		return fmt.Errorf("read qmp_capabilities response: %w", err)
	}
	slog.Debug("qmp capabilities response", "data", string(buf[:n]))
	return nil
}

// qmpCommand sends a simple QMP command (no arguments).
func qmpCommand(sockPath, command string) error {
	conn, err := net.Dial("unix", sockPath)
	if err != nil {
		return err
	}
	defer conn.Close()

	conn.SetDeadline(time.Now().Add(5 * time.Second))

	// Read greeting.
	buf := make([]byte, 4096)
	if _, err := conn.Read(buf); err != nil {
		return fmt.Errorf("read greeting: %w", err)
	}

	// Send qmp_capabilities (required before any command).
	if _, err := conn.Write([]byte(`{"execute":"qmp_capabilities"}` + "\n")); err != nil {
		return err
	}
	if _, err := conn.Read(buf); err != nil {
		return fmt.Errorf("read capabilities response: %w", err)
	}

	// Send the actual command.
	msg, _ := json.Marshal(map[string]string{"execute": command})
	if _, err := conn.Write(append(msg, '\n')); err != nil {
		return err
	}

	// Read response.
	n, err := conn.Read(buf)
	if err != nil {
		return fmt.Errorf("read %s response: %w", command, err)
	}

	resp := string(buf[:n])
	if strings.Contains(resp, `"error"`) {
		return fmt.Errorf("qmp %s: %s", command, resp)
	}
	return nil
}

func qemuLogTail(dir string) string {
	data, err := os.ReadFile(filepath.Join(dir, "qemu.log"))
	if err != nil || len(data) == 0 {
		return "qemu.log: (not available)"
	}
	lines := strings.Split(strings.TrimSpace(string(data)), "\n")
	const maxLines = 20
	if len(lines) > maxLines {
		lines = lines[len(lines)-maxLines:]
	}
	return "qemu.log:\n" + strings.Join(lines, "\n")
}

func init() {
	forkQemuVMFunc = ForkQemuVM
}

// ForkQemuVM snapshots a running QEMU VM and creates a clone that can be
// booted with RunDaemon. The clone's rootfs and RAM are APFS clonefiles
// (CoW) of the original. The VM is left paused — the caller must resume
// it with QMP cont after any additional fixups (e.g. cloning the active
// RAM file).
//
// Returns (true, nil) if QEMU exited during migration (no resume needed),
// or (false, nil) if the VM is still alive and paused.
//
// qmpSock is the path to the running VM's QMP socket.
// srcDir contains rootfs.ext4 and ram.img.
// dstDir receives the cloned files (rootfs, ram, incoming.bin, vmlinuz).
func ForkQemuVM(qmpSock, srcDir, dstDir string) (exited bool, err error) {

	// Use a single QMP session for stop + migrate.
	conn, err := net.Dial("unix", qmpSock)
	if err != nil {
		return false, fmt.Errorf("qmp connect: %w", err)
	}
	defer conn.Close()
	conn.SetDeadline(time.Now().Add(30 * time.Second))

	buf := make([]byte, 8192)
	conn.Read(buf) // greeting
	conn.Write([]byte(`{"execute":"qmp_capabilities"}` + "\n"))
	conn.Read(buf)

	// 1. Pause the VM.
	conn.Write([]byte(`{"execute":"stop"}` + "\n"))
	time.Sleep(200 * time.Millisecond)
	conn.Read(buf)

	// 2. Save CPU/device state via QMP migrate (CPR-reboot mode).
	// x-ignore-shared skips RAM (already in the shared memory-backend-file).
	// CPR-reboot mode preserves the VM state for restart — the VM stays
	// paused after migration and the caller resumes it.
	stateFile := filepath.Join(dstDir, "incoming.bin")

	conn.Write([]byte(`{"execute":"migrate-set-capabilities","arguments":{"capabilities":[{"capability":"x-ignore-shared","state":true}]}}` + "\n"))
	time.Sleep(100 * time.Millisecond)
	conn.Read(buf)

	conn.Write([]byte(`{"execute":"migrate-set-parameters","arguments":{"mode":"cpr-reboot"}}` + "\n"))
	time.Sleep(100 * time.Millisecond)
	conn.Read(buf)

	migrateCmd, _ := json.Marshal(map[string]any{
		"execute":   "migrate",
		"arguments": map[string]string{"uri": "file:" + stateFile},
	})
	conn.Write(append(migrateCmd, '\n'))
	time.Sleep(200 * time.Millisecond)
	conn.Read(buf)

	// Wait for migration to complete. QEMU may exit after migration
	// depending on state (e.g. active vsock connections). If the QMP
	// connection dies, check whether the state file was written.
	qemuExited := false
	for i := 0; i < 60; i++ {
		time.Sleep(500 * time.Millisecond)
		conn.SetDeadline(time.Now().Add(5 * time.Second))
		conn.Write([]byte(`{"execute":"query-migrate"}` + "\n"))
		n, err := conn.Read(buf)
		if err != nil {
			// QMP connection died. Verify the state file was written.
			for j := 0; j < 10; j++ {
				if info, statErr := os.Stat(stateFile); statErr == nil && info.Size() > 0 {
					qemuExited = true
					goto migrated
				}
				time.Sleep(100 * time.Millisecond)
			}
			return false, fmt.Errorf("query-migrate: %w", err)
		}
		resp := string(buf[:n])
		if strings.Contains(resp, `"completed"`) {
			goto migrated
		}
		if strings.Contains(resp, `"failed"`) {
			conn.Write([]byte(`{"execute":"cont"}` + "\n"))
			return false, fmt.Errorf("migration failed: %s", resp)
		}
	}
	conn.Write([]byte(`{"execute":"cont"}` + "\n"))
	return false, fmt.Errorf("migration timed out")

migrated:
	// 3. Clone rootfs + ram via APFS clonefile.
	for _, name := range []string{"rootfs.ext4", "ram.img"} {
		src := filepath.Join(srcDir, name)
		if _, err := os.Stat(src); err != nil {
			continue
		}
		if err := cloneFile(src, filepath.Join(dstDir, name)); err != nil {
			if !qemuExited {
				conn.Write([]byte(`{"execute":"cont"}` + "\n"))
			}
			return qemuExited, fmt.Errorf("clone %s: %w", name, err)
		}
	}

	// 4. Copy kernel symlink.
	if target, err := os.Readlink(filepath.Join(srcDir, "vmlinuz")); err == nil {
		os.Symlink(target, filepath.Join(dstDir, "vmlinuz"))
	}

	// VM is left paused (unless QEMU exited) — caller sends cont after fixups.
	return qemuExited, nil
}
