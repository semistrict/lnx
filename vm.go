package lnx

import (
	"encoding/gob"
	"fmt"
	"log/slog"
	"net"
	"os"
	"os/signal"
	"os/user"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	vz "github.com/Code-Hex/vz/v3"
	"golang.org/x/sys/unix"
	"golang.org/x/term"

	"github.com/semistrict/lnx/internal/protocol"
)

const vsockLogPort = 1025

var hostLogOnce sync.Once

// Run executes a command inside a Linux VM and blocks until it exits.
// args is a command vector (like exec): args[0] is the program, args[1:] are arguments.
// Returns the guest process exit code.
func Run(cfg *Config, args ...string) (int, error) {
	initHostLoggingFromEnv()

	if err := validatePaths(cfg); err != nil {
		return -1, err
	}

	if cfg.Ephemeral {
		tmpDir, err := os.MkdirTemp("", "lnx-ephemeral-*")
		if err != nil {
			return -1, fmt.Errorf("create ephemeral dir: %w", err)
		}
		defer os.RemoveAll(tmpDir)

		ephRootfs := filepath.Join(tmpDir, "rootfs.ext4")
		if err := unix.Clonefile(cfg.RootfsPath, ephRootfs, 0); err != nil {
			return -1, fmt.Errorf("clone ephemeral rootfs: %w", err)
		}
		cfg = &Config{
			KernelPath:    cfg.KernelPath,
			RootfsPath:    ephRootfs,
			InitramfsPath: cfg.InitramfsPath,
			CPUs:          cfg.CPUs,
			MemoryBytes:   cfg.MemoryBytes,
			CWD:           cfg.CWD,
			Env:           cfg.Env,
			Checkpoint:    cfg.Checkpoint,
			CheckpointDir: cfg.CheckpointDir,
			Shares:        cfg.Shares,
			Hostname:      cfg.Hostname,
			SSHAgent:      cfg.SSHAgent,
		}
	}

	lock, err := lockRootfs(cfg.RootfsPath)
	if err != nil {
		return -1, err
	}
	defer lock.unlock()

	if cfg.Checkpoint {
		cpDir := cfg.CheckpointDir
		if cpDir == "" {
			cpDir = filepath.Join(filepath.Dir(cfg.RootfsPath), "checkpoints")
		}
		cpPath, err := checkpoint(cfg.RootfsPath, cpDir)
		if err != nil {
			return -1, fmt.Errorf("checkpoint: %w", err)
		}
		fmt.Fprintf(os.Stderr, "checkpoint: %s\n", cpPath)
	}

	initrdDir := filepath.Dir(cfg.RootfsPath)
	if cfg.InitramfsPath != "" {
		initrdDir = filepath.Dir(cfg.InitramfsPath)
	}
	initrdPath, err := writeInitramfs(initrdDir)
	if err != nil {
		return -1, fmt.Errorf("write initramfs: %w", err)
	}

	cwd := cfg.CWD
	if cwd == "" {
		cwd, err = os.Getwd()
		if err != nil {
			return -1, fmt.Errorf("getwd: %w", err)
		}
	}

	u, err := user.Current()
	if err != nil {
		return -1, fmt.Errorf("get current user: %w", err)
	}
	uid, _ := strconv.Atoi(u.Uid)

	swapPath := filepath.Join(filepath.Dir(cfg.RootfsPath), "swap.img")
	if err := ensureSwapFile(swapPath, cfg.memoryBytes()); err != nil {
		return -1, fmt.Errorf("swap file: %w", err)
	}

	vmConfig, err := buildVMConfig(cfg, initrdPath, cwd, swapPath, u.HomeDir)
	if err != nil {
		return -1, err
	}

	// Auto-detect interactive mode based on whether stdin is a terminal.
	interactive := term.IsTerminal(int(os.Stdin.Fd()))

	vm, err := vz.NewVirtualMachine(vmConfig)
	if err != nil {
		return -1, fmt.Errorf("create vm: %w", err)
	}

	hostname := cfg.Hostname
	if hostname == "" {
		hostname = "lnx"
	}

	sshAgent := cfg.SSHAgent && os.Getenv("SSH_AUTH_SOCK") != ""
	if cfg.SSHAgent && !sshAgent {
		fmt.Fprintln(os.Stderr, "warning: --ssh-agent requested but SSH_AUTH_SOCK is not set")
	}
	if sshAgent {
		if n, err := countSSHKeys(os.Getenv("SSH_AUTH_SOCK")); err != nil {
			fmt.Fprintf(os.Stderr, "warning: cannot query SSH agent: %v\n", err)
		} else if n == 0 {
			fmt.Fprintln(os.Stderr, "warning: SSH agent has no identities loaded — run 'ssh-add' on the host")
		}
	}

	setupMsg := &protocol.Setup{
		CWD:      cwd,
		Env:      buildGuestEnv(cfg.Env),
		User:     u.Username,
		UID:      uid,
		HomeDir:  u.HomeDir,
		Hostname: hostname,
		SSHAgent: sshAgent,
		Shares:   cfg.Shares,
	}

	vs, err := setupVsock(vm, filepath.Dir(cfg.RootfsPath), cfg.RootfsPath, setupMsg)
	if err != nil {
		return -1, err
	}
	if err := vm.Start(); err != nil {
		vs.cleanup()
		return -1, fmt.Errorf("start vm: %w", err)
	}

	// Wait for control connection, send setup message.
	ctrlConn := <-vs.ctrlConnCh
	if ctrlConn == nil {
		vs.cleanup()
		return -1, fmt.Errorf("control connection failed")
	}
	enc := gob.NewEncoder(ctrlConn)
	if err := enc.Encode(protocol.Msg{Setup: setupMsg}); err != nil {
		ctrlConn.Close()
		vs.cleanup()
		return -1, fmt.Errorf("send setup: %w", err)
	}
	forceQuitCh := make(chan struct{})
	go forwardSignals(ctrlConn, enc, forceQuitCh)

	// Run the command through the exec path — same as `lnx exec`.
	var rows, cols uint16
	if interactive {
		fd := int(os.Stdin.Fd())
		if term.IsTerminal(fd) {
			w, h, err := term.GetSize(fd)
			if err == nil {
				rows = uint16(h)
				cols = uint16(w)
			}
			oldState, err := term.MakeRaw(fd)
			if err == nil {
				defer term.Restore(fd, oldState)
			}
		}
	}

	exitCode := vs.api.runExec(&protocol.ExecReq{
		Args: args,
		CWD:  cwd,
		PTY:  interactive,
		Rows: rows,
		Cols: cols,
	}, interactive, forceQuitCh)

	// Check if force quit happened (double Ctrl-C).
	select {
	case <-forceQuitCh:
		exitCode = 130
	default:
	}

	// Shut down the VM by closing the control connection.
	ctrlConn.Close()

	return shutdownVM(vm, exitCode, vs.cleanup)
}

// vsockState holds the vsock infrastructure created during VM setup.
type vsockState struct {
	ctrlConnCh <-chan net.Conn
	api        *apiServer
	cleanup    func()
}

func buildVMConfig(cfg *Config, initrdPath, cwd, swapPath, homeDir string) (*vz.VirtualMachineConfiguration, error) {
	cmdline := fmt.Sprintf("console=hvc0 quiet lnx.epoch=%d", time.Now().Unix())

	bootLoader, err := vz.NewLinuxBootLoader(
		cfg.KernelPath,
		vz.WithCommandLine(cmdline),
		vz.WithInitrd(initrdPath),
	)
	if err != nil {
		return nil, fmt.Errorf("boot loader: %w", err)
	}

	vmConfig, err := vz.NewVirtualMachineConfiguration(bootLoader, cfg.cpus(), cfg.memoryBytes())
	if err != nil {
		return nil, fmt.Errorf("vm config: %w", err)
	}

	for _, attach := range []func(*vz.VirtualMachineConfiguration) error{
		attachSerial,
		func(c *vz.VirtualMachineConfiguration) error { return attachDisks(c, cfg.RootfsPath, swapPath) },
		func(c *vz.VirtualMachineConfiguration) error { return attachShares(c, cwd, cfg.Shares) },
		attachNetwork,
		attachMisc,
	} {
		if err := attach(vmConfig); err != nil {
			return nil, err
		}
	}

	vsockConfig, err := vz.NewVirtioSocketDeviceConfiguration()
	if err != nil {
		return nil, fmt.Errorf("vsock config: %w", err)
	}
	vmConfig.SetSocketDevicesVirtualMachineConfiguration([]vz.SocketDeviceConfiguration{vsockConfig})

	if ok, err := vmConfig.Validate(); !ok || err != nil {
		return nil, fmt.Errorf("validate config: %w", err)
	}

	return vmConfig, nil
}

func setupVsock(vm *vz.VirtualMachine, logDir, rootfsPath string, setupMsg *protocol.Setup) (*vsockState, error) {
	socketDevices := vm.SocketDevices()
	if len(socketDevices) == 0 {
		return nil, fmt.Errorf("no vsock devices")
	}
	sock := socketDevices[0]

	logListener, err := sock.Listen(vsockLogPort)
	if err != nil {
		return nil, fmt.Errorf("vsock log listen: %w", err)
	}
	waitLog := startLogReceiver(logListener, logDir)

	ctrlListener, err := sock.Listen(protocol.Port)
	if err != nil {
		logListener.Close()
		return nil, fmt.Errorf("vsock ctrl listen: %w", err)
	}

	statusListener, err := sock.Listen(protocol.StatusPort)
	if err != nil {
		ctrlListener.Close()
		logListener.Close()
		return nil, fmt.Errorf("vsock status listen: %w", err)
	}

	guestCtrlListener, err := sock.Listen(protocol.GuestControlPort)
	if err != nil {
		statusListener.Close()
		ctrlListener.Close()
		logListener.Close()
		return nil, fmt.Errorf("vsock guest ctrl listen: %w", err)
	}

	portFwdListener, err := sock.Listen(protocol.PortForwardPort)
	if err != nil {
		guestCtrlListener.Close()
		statusListener.Close()
		ctrlListener.Close()
		logListener.Close()
		return nil, fmt.Errorf("vsock port forward listen: %w", err)
	}

	// 9P file server for home directory.
	p9Listener, err := sock.Listen(protocol.P9Port)
	if err != nil {
		portFwdListener.Close()
		guestCtrlListener.Close()
		statusListener.Close()
		ctrlListener.Close()
		logListener.Close()
		return nil, fmt.Errorf("vsock 9p listen: %w", err)
	}
	start9PServer(p9Listener, setupMsg.HomeDir)

	var sshAgentListener net.Listener
	if setupMsg.SSHAgent {
		var err error
		sshAgentListener, err = sock.Listen(protocol.SSHAgentPort)
		if err != nil {
			slog.Warn("ssh agent vsock listen failed", "error", err)
		} else {
			startSSHAgentProxy(sshAgentListener, os.Getenv("SSH_AUTH_SOCK"))
		}
	}

	pf := newPortForwarder(sock)
	go func() {
		conn, err := portFwdListener.Accept()
		if err != nil {
			return
		}
		pf.run(conn)
	}()

	api := newAPIServer(nil, setupMsg.User, rootfsPath)
	api.sock = sock
	api.pf = pf
	go func() {
		conn, err := statusListener.Accept()
		if err != nil {
			return
		}
		api.setStatusConn(conn)
	}()
	go func() {
		conn, err := guestCtrlListener.Accept()
		if err != nil {
			return
		}
		api.setGuestCtrlConn(conn)
	}()

	sockPath := filepath.Join(logDir, "status.sock")
	if err := api.listenUnix(sockPath); err != nil {
		slog.Warn("status socket failed", "error", err)
	}

	// Accept control connection asynchronously (VM hasn't started yet).
	ctrlConnCh := make(chan net.Conn, 1)
	go func() {
		conn, err := ctrlListener.Accept()
		if err != nil {
			return
		}
		ctrlConnCh <- conn
	}()

	cleanup := func() {
		pf.close()
		api.close()
		if sshAgentListener != nil {
			sshAgentListener.Close()
		}
		p9Listener.Close()
		portFwdListener.Close()
		guestCtrlListener.Close()
		statusListener.Close()
		ctrlListener.Close()
		logListener.Close()
		waitLog()
	}

	return &vsockState{ctrlConnCh: ctrlConnCh, api: api, cleanup: cleanup}, nil
}

// forwardSignals reads host signals and forwards them to the guest via the
// control connection. SIGWINCH is converted to Resize. Double SIGINT
// force-quits.
func forwardSignals(conn net.Conn, enc *gob.Encoder, forceQuitCh chan struct{}) {
	sigCh := make(chan os.Signal, 4)
	signal.Notify(sigCh, syscall.SIGTERM, syscall.SIGINT, syscall.SIGHUP, syscall.SIGWINCH)
	defer signal.Stop(sigCh)

	var lastInt time.Time
	for sig := range sigCh {
		if sig == syscall.SIGWINCH {
			w, h, err := term.GetSize(int(os.Stdin.Fd()))
			if err == nil {
				enc.Encode(protocol.Msg{Resize: &protocol.Resize{
					Rows: uint16(h),
					Cols: uint16(w),
				}})
			}
		} else if sig == syscall.SIGINT && time.Since(lastInt) < time.Second {
			fmt.Fprintln(os.Stderr, "\nforce quit")
			close(forceQuitCh)
			conn.Close()
			return
		} else {
			if sig == syscall.SIGINT {
				lastInt = time.Now()
			}
			enc.Encode(protocol.Msg{Signal: &protocol.Signal{
				Sig: int(sig.(syscall.Signal)),
			}})
		}
	}
}

func shutdownVM(vm *vz.VirtualMachine, exitCode int, cleanup func()) (int, error) {
	defer cleanup()

	if exitCode == 130 {
		vm.Stop()
		return exitCode, nil
	}

	vm.RequestStop()
	stateCh := vm.StateChangedNotify()
	select {
	case <-time.After(3 * time.Second):
		vm.Stop()
	case state := <-stateCh:
		if state != vz.VirtualMachineStateStopped {
			vm.Stop()
		}
	}
	return exitCode, nil
}

func validatePaths(cfg *Config) error {
	for _, p := range []string{cfg.KernelPath, cfg.RootfsPath} {
		if _, err := os.Stat(p); err != nil {
			return fmt.Errorf("%s not found, run 'lnx init' first", p)
		}
	}
	return nil
}

func initHostLoggingFromEnv() {
	hostLogOnce.Do(func() {
		level := slog.LevelInfo
		switch strings.ToLower(os.Getenv("LNX_LOG")) {
		case "debug":
			level = slog.LevelDebug
		case "warn":
			level = slog.LevelWarn
		case "error":
			level = slog.LevelError
		}

		home, err := os.UserHomeDir()
		if err != nil {
			return
		}
		logDir := filepath.Join(home, ".lnx")
		os.MkdirAll(logDir, 0755)
		f, err := os.OpenFile(filepath.Join(logDir, "lnx.log"), os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0644)
		if err != nil {
			return
		}
		slog.SetDefault(slog.New(slog.NewTextHandler(f, &slog.HandlerOptions{Level: level})))
	})
}

// buildGuestEnv filters the host environment and merges in extra vars.
func buildGuestEnv(extra []string) []string {
	var env []string
	for _, kv := range os.Environ() {
		key, _, _ := strings.Cut(kv, "=")
		if excludeEnvKey(key) {
			continue
		}
		env = append(env, kv)
	}
	env = append(env, extra...)
	return env
}

// ensureSwapFile creates a sparse swap file if it doesn't exist or is the wrong size.
func ensureSwapFile(path string, size uint64) error {
	if info, err := os.Stat(path); err == nil && uint64(info.Size()) == size {
		return nil
	}
	f, err := os.Create(path)
	if err != nil {
		return err
	}
	defer f.Close()
	return f.Truncate(int64(size))
}

func excludeEnvKey(key string) bool {
	switch key {
	case "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "TERM",
		"SSH_AUTH_SOCK", "DISPLAY",
		"SECURITYSESSIONID", "LaunchInstanceID", "COMMAND_MODE",
		"LANG", "LC_ALL", "LC_CTYPE", "LC_COLLATE", "LC_MESSAGES",
		"LC_MONETARY", "LC_NUMERIC", "LC_TIME":
		return true
	}
	for _, prefix := range []string{"__CF_", "APPLE_", "XPC_"} {
		if strings.HasPrefix(key, prefix) {
			return true
		}
	}
	return false
}
