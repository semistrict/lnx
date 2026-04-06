package lnx

import (
	"fmt"
	"log/slog"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
)

// useQEMU returns true when the qemubackend experiment is enabled.
func useQEMU() bool {
	return strings.Contains(os.Getenv("LNX_EXPERIMENTS"), "qemubackend")
}

// qemuBinary returns the path to qemu-system-aarch64.
// Checks QEMU_PATH env, then falls back to PATH lookup.
func qemuBinary() (string, error) {
	if p := os.Getenv("QEMU_PATH"); p != "" {
		if _, err := os.Stat(p); err == nil {
			return p, nil
		}
	}
	return exec.LookPath("qemu-system-aarch64")
}

// qemuVM holds a running QEMU process and its vsock interface.
type qemuVM struct {
	cmd        *exec.Cmd
	socketPath string
}

func (q *qemuVM) shutdown(exitCode int) {
	if q.cmd != nil && q.cmd.Process != nil {
		q.cmd.Process.Signal(os.Interrupt)
		done := make(chan error, 1)
		go func() { done <- q.cmd.Wait() }()
		select {
		case <-done:
		case <-time.After(3 * time.Second):
			q.cmd.Process.Kill()
			<-done
		}
	}
	os.Remove(q.socketPath)
}

// buildQEMUArgs constructs the qemu-system-aarch64 command line.
func buildQEMUArgs(cfg *Config, initrdPath, cwd, swapPath string, epoch int64) ([]string, string, error) {
	bin, err := qemuBinary()
	if err != nil {
		return nil, "", fmt.Errorf("qemu-system-aarch64 not found: %w", err)
	}

	vsockPath := filepath.Join(cfg.socketDir(), "qemu.vsock")

	cmdline := fmt.Sprintf("console=hvc0 quiet lnx.epoch=%d", epoch)

	args := []string{
		bin,
		"-accel", "hvf",
		"-cpu", "host",
		"-M", "virt",
		"-smp", fmt.Sprintf("%d", cfg.cpus()),
		"-m", fmt.Sprintf("%dM", cfg.memoryBytes()/(1024*1024)),
		"-kernel", cfg.KernelPath,
		"-initrd", initrdPath,
		"-append", cmdline,
		"-nographic",
		"-nodefaults",

		// Serial console (hvc0) to /dev/null — all I/O goes via vsock.
		"-chardev", "null,id=hvc0",
		"-device", "virtio-serial-device",
		"-device", "virtconsole,chardev=hvc0",

		// Root disk (vda).
		"-drive", fmt.Sprintf("file=%s,format=raw,if=none,id=root", cfg.RootfsPath),
		"-device", "virtio-blk-device,drive=root",

		// Swap disk (vdb).
		"-drive", fmt.Sprintf("file=%s,format=raw,if=none,id=swap", swapPath),
		"-device", "virtio-blk-device,drive=swap",

		// Networking (user-mode NAT).
		"-netdev", "user,id=net0",
		"-device", "virtio-net-device,netdev=net0",

		// Vsock (pure in-process, no vhost).
		"-device", fmt.Sprintf("virtio-vsock-pci,guest-cid=3,socket-path=%s", vsockPath),

		// RNG.
		"-device", "virtio-rng-pci",

		// Balloon.
		"-device", "virtio-balloon-device",

		"-no-reboot",
	}

	// CWD share via 9P (QEMU built-in).
	args = append(args,
		"-fsdev", fmt.Sprintf("local,security_model=none,id=cwd_dev,path=%s", cwd),
		"-device", "virtio-9p-pci,fsdev=cwd_dev,mount_tag=cwd",
	)

	// Extra shares via 9P.
	for i, path := range cfg.Shares {
		tag := fmt.Sprintf("share%d", i)
		id := fmt.Sprintf("share_dev_%d", i)
		args = append(args,
			"-fsdev", fmt.Sprintf("local,security_model=none,id=%s,path=%s", id, path),
			"-device", fmt.Sprintf("virtio-9p-pci,fsdev=%s,mount_tag=%s", id, tag),
		)
	}

	return args, vsockPath, nil
}

// startQEMU launches qemu-system-aarch64 and waits for the vsock listener
// socket to appear (indicating the device is ready).
func startQEMU(args []string, vsockPath string) (*qemuVM, error) {
	slog.Info("starting QEMU", "binary", args[0])

	cmd := exec.Command(args[0], args[1:]...)
	cmd.Stdout = nil
	cmd.Stderr = nil

	if err := cmd.Start(); err != nil {
		return nil, fmt.Errorf("start qemu: %w", err)
	}

	// Wait for the vsock listener socket to appear.
	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		if _, err := os.Stat(vsockPath); err == nil {
			slog.Info("QEMU vsock ready", "path", vsockPath)
			return &qemuVM{cmd: cmd, socketPath: vsockPath}, nil
		}
		time.Sleep(50 * time.Millisecond)
	}

	cmd.Process.Kill()
	cmd.Wait()
	return nil, fmt.Errorf("timed out waiting for QEMU vsock socket at %s", vsockPath)
}

// qemuVsock implements VsockDevice for the QEMU backend.
// Guest-to-host: QEMU connects to {socketPath}_{port} when guest dials host.
// Host-to-guest: host connects to {socketPath} and sends "CONNECT {port}\n".
type qemuVsock struct {
	socketPath string
}

func newQEMUVsock(socketPath string) VsockDevice {
	return &qemuVsock{socketPath: socketPath}
}

// Listen creates a Unix socket at {socketPath}_{port} for guest-to-host connections.
// When the guest connects to host CID on this port, QEMU connects to this socket.
func (q *qemuVsock) Listen(port uint32) (net.Listener, error) {
	path := fmt.Sprintf("%s_%d", q.socketPath, port)
	os.Remove(path) // clean up stale socket
	ln, err := net.Listen("unix", path)
	if err != nil {
		return nil, fmt.Errorf("listen on %s: %w", path, err)
	}
	return ln, nil
}

// Connect initiates a host-to-guest connection. Connects to QEMU's listener
// socket and sends "CONNECT {port}\n" to request a vsock connection to the guest.
func (q *qemuVsock) Connect(port uint32) (net.Conn, error) {
	conn, err := net.Dial("unix", q.socketPath)
	if err != nil {
		return nil, fmt.Errorf("dial qemu vsock: %w", err)
	}
	header := fmt.Sprintf("CONNECT %d\n", port)
	if _, err := conn.Write([]byte(header)); err != nil {
		conn.Close()
		return nil, fmt.Errorf("send connect header: %w", err)
	}
	return conn, nil
}
