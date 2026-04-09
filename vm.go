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

	"golang.org/x/term"

	"github.com/semistrict/lnx/internal/protocol"
)

const vsockLogPort = 1025

var hostLogOnce sync.Once

// bootedVM holds a booted VM and its infrastructure, ready for exec sessions.
type bootedVM struct {
	vm       VirtualMachine
	vs       *vsockState
	ctrlConn net.Conn
	ctrlEnc  *gob.Encoder
	cwd      string
	cfg      *Config
	initrd   string
	swapPath string
	setup    *protocol.Setup
	// ephCleanup removes the ephemeral temp dir (nil if not ephemeral).
	ephCleanup func()
	lock       *lockFile
}

// close shuts down the VM and releases all resources.
func (b *bootedVM) close(exitCode int) {
	if b.ctrlConn != nil {
		b.ctrlConn.Close()
	}
	shutdownVM(b.vm, exitCode)
	b.lock.unlock()
	b.vs.cleanup()
	if b.ephCleanup != nil {
		b.ephCleanup()
	}
}

// bootVM handles the shared VM boot sequence: validate, ephemeral clone, lock,
// checkpoint, initramfs, VM config, start, control connection, setup message.
func bootVM(cfg *Config) (*bootedVM, error) {
	initHostLoggingFromEnv()

	if err := validatePaths(cfg); err != nil {
		return nil, err
	}

	var ephCleanup func()
	if cfg.Ephemeral {
		tmpDir, err := os.MkdirTemp("", "lnx-ephemeral-*")
		if err != nil {
			return nil, fmt.Errorf("create ephemeral dir: %w", err)
		}
		ephCleanup = func() { os.RemoveAll(tmpDir) }

		ephRootfs := filepath.Join(tmpDir, "rootfs.ext4")
		if err := cloneFile(cfg.RootfsPath, ephRootfs); err != nil {
			ephCleanup()
			return nil, fmt.Errorf("clone ephemeral rootfs: %w", err)
		}
		cfg = &Config{
			KernelPath:    cfg.KernelPath,
			RootfsPath:    ephRootfs,
			InitramfsPath: cfg.InitramfsPath,
			CommandLine:   cfg.CommandLine,
			CPUs:          cfg.CPUs,
			MemoryBytes:   cfg.MemoryBytes,
			CWD:           cfg.CWD,
			Env:           cfg.Env,
			Checkpoint:    cfg.Checkpoint,
			CheckpointDir: cfg.CheckpointDir,
			Shares:        cfg.Shares,
			Hostname:      cfg.Hostname,
			SSHAgent:      cfg.SSHAgent,
			SocketDir:     cfg.SocketDir,
			NestedRootfs:  cfg.NestedRootfs,
		}
	}

	lock, err := lockRootfs(cfg.RootfsPath)
	if err != nil {
		if ephCleanup != nil {
			ephCleanup()
		}
		return nil, err
	}

	if cfg.Checkpoint {
		cpDir := cfg.CheckpointDir
		if cpDir == "" {
			cpDir = filepath.Join(filepath.Dir(cfg.RootfsPath), "checkpoints")
		}
		if _, err := checkpoint(cfg.RootfsPath, cpDir); err != nil {
			lock.unlock()
			if ephCleanup != nil {
				ephCleanup()
			}
			return nil, fmt.Errorf("checkpoint: %w", err)
		}
	}

	// Use socketDir as the work directory for derived files when rootfs
	// is a block device (filepath.Dir("/dev/vdc") = "/dev", not writable).
	workDir := filepath.Dir(cfg.RootfsPath)
	if strings.HasPrefix(cfg.RootfsPath, "/dev/") {
		workDir = cfg.socketDir()
	}

	initrdDir := workDir
	if cfg.InitramfsPath != "" {
		initrdDir = filepath.Dir(cfg.InitramfsPath)
	}
	initrdPath, err := writeInitramfs(initrdDir)
	if err != nil {
		lock.unlock()
		if ephCleanup != nil {
			ephCleanup()
		}
		return nil, fmt.Errorf("write initramfs: %w", err)
	}

	cwd := cfg.CWD
	if cwd == "" {
		cwd, err = os.Getwd()
		if err != nil {
			lock.unlock()
			if ephCleanup != nil {
				ephCleanup()
			}
			return nil, fmt.Errorf("getwd: %w", err)
		}
	}

	u, err := user.Current()
	if err != nil {
		lock.unlock()
		if ephCleanup != nil {
			ephCleanup()
		}
		return nil, fmt.Errorf("get current user: %w", err)
	}
	uid, _ := strconv.Atoi(u.Uid)

	swapPath := filepath.Join(workDir, "swap.img")
	if err := ensureSwapFile(swapPath, cfg.memoryBytes()); err != nil {
		lock.unlock()
		if ephCleanup != nil {
			ephCleanup()
		}
		return nil, fmt.Errorf("swap file: %w", err)
	}

	hostname := cfg.Hostname
	if hostname == "" {
		hostname = "lnx"
	}
	if cfg.CommandLine == "" {
		cfg.CommandLine = fmt.Sprintf("console=hvc0 lnx.epoch=%d", time.Now().Unix())
	}

	sshAgent := cfg.SSHAgent && os.Getenv("SSH_AUTH_SOCK") != ""
	if cfg.SSHAgent && !sshAgent {
		slog.Warn("--ssh-agent requested but SSH_AUTH_SOCK is not set")
	}
	if sshAgent {
		if n, err := countSSHKeys(os.Getenv("SSH_AUTH_SOCK")); err != nil {
			slog.Warn("cannot query SSH agent", "error", err)
		} else if n == 0 {
			slog.Warn("SSH agent has no identities loaded")
		}
	}

	// Pass LNX_PARENT so nested lnx instances know their parent.
	parentInstance := os.Getenv("LNX_INSTANCE")
	if parentInstance == "" {
		parentInstance = "default"
	}
	if existing := os.Getenv("LNX_PARENT"); existing != "" {
		parentInstance = existing + "." + parentInstance
	}

	// Build nested drive mapping: each nested rootfs gets a device starting at vdc.
	var nestedDrives []protocol.NestedDrive
	for i, nr := range cfg.NestedRootfs {
		devLetter := 'c' + rune(i) // vdc, vdd, vde, ...
		nestedDrives = append(nestedDrives, protocol.NestedDrive{
			InstanceName: nr.InstanceName,
			DevicePath:   fmt.Sprintf("/dev/vd%c", devLetter),
		})
	}

	setupMsg := &protocol.Setup{
		CWD:          cwd,
		Env:          append([]string(nil), cfg.Env...),
		User:         u.Username,
		UID:          uid,
		HomeDir:      u.HomeDir,
		Hostname:     hostname,
		SSHAgent:     sshAgent,
		Shares:       cfg.Shares,
		ShareMethod:  shareMethod(),
		NestedDrives: nestedDrives,
	}
	setupMsg.Env = append(setupMsg.Env, "LNX_PARENT="+parentInstance)

	vm, err := buildVM(cfg, initrdPath, cwd, swapPath, u.HomeDir)
	if err != nil {
		lock.unlock()
		if ephCleanup != nil {
			ephCleanup()
		}
		return nil, err
	}

	sockDir := cfg.socketDir()
	vs, err := setupVsock(vm.VsockDevice(), sockDir, cfg.RootfsPath, setupMsg)
	if err != nil {
		lock.unlock()
		if ephCleanup != nil {
			ephCleanup()
		}
		return nil, err
	}
	if err := vm.Start(); err != nil {
		vs.cleanup()
		lock.unlock()
		if ephCleanup != nil {
			ephCleanup()
		}
		return nil, fmt.Errorf("start vm: %w", err)
	}

	// Wait for the guest to connect on the control port, with a timeout.
	// Also watch for VM state changes (crash/stop) so we don't wait forever.
	var ctrlConn net.Conn
	stateCh := vm.StateChangedNotify()
	bootTimer := time.NewTimer(30 * time.Second)
	defer bootTimer.Stop()
waitBoot:
	for {
		select {
		case ctrlConn = <-vs.ctrlConnCh:
			break waitBoot
		case state := <-stateCh:
			switch state {
			case VMStateRunning, VMStateStarting:
				continue // expected transient states
			default:
				vs.cleanup()
				lock.unlock()
				if ephCleanup != nil {
					ephCleanup()
				}
				return nil, fmt.Errorf("VM entered state %v during boot\n%s", state, serialLogTail(sockDir))
			}
		case <-bootTimer.C:
			vs.cleanup()
			vm.Stop()
			lock.unlock()
			if ephCleanup != nil {
				ephCleanup()
			}
			return nil, fmt.Errorf("guest did not connect within 30s\n%s", serialLogTail(sockDir))
		}
	}
	if ctrlConn == nil {
		vs.cleanup()
		lock.unlock()
		if ephCleanup != nil {
			ephCleanup()
		}
		return nil, fmt.Errorf("control connection failed\n%s", serialLogTail(sockDir))
	}
	enc := gob.NewEncoder(ctrlConn)
	if err := enc.Encode(protocol.Msg{Setup: setupMsg}); err != nil {
		ctrlConn.Close()
		vs.cleanup()
		lock.unlock()
		if ephCleanup != nil {
			ephCleanup()
		}
		return nil, fmt.Errorf("send setup: %w", err)
	}

	return &bootedVM{
		vm:         vm,
		vs:         vs,
		ctrlConn:   ctrlConn,
		ctrlEnc:    enc,
		cwd:        cwd,
		cfg:        cfg,
		initrd:     initrdPath,
		swapPath:   swapPath,
		setup:      setupMsg,
		ephCleanup: ephCleanup,
		lock:       lock,
	}, nil
}

// Run executes a command inside a Linux VM and blocks until it exits.
// args is a command vector (like exec): args[0] is the program, args[1:] are arguments.
// Returns the guest process exit code.
func Run(cfg *Config, args ...string) (int, error) {
	b, err := bootVM(cfg)
	if err != nil {
		return -1, err
	}

	forceQuitCh := make(chan struct{})
	go forwardSignals(b.ctrlConn, b.ctrlEnc, forceQuitCh)

	// Auto-detect interactive mode based on whether stdin is a terminal.
	interactive := term.IsTerminal(int(os.Stdin.Fd()))

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

	exitCode := b.vs.api.runExec(&protocol.ExecReq{
		Args: args,
		CWD:  b.cwd,
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

	b.close(exitCode)
	return exitCode, nil
}

// RunDaemon boots a VM and runs it as a background daemon with no initial command.
// It blocks until all exec sessions have finished (idle) or Stop is requested
// via the API. Returns nil on clean shutdown.
func RunDaemon(cfg *Config) error {
	if cfg.Restore != nil {
		return runRestoredDaemon(cfg)
	}

	b, err := bootVM(cfg)
	if err != nil {
		return err
	}
	if vm, ok := b.vm.(SnapshotCapableVirtualMachine); ok && experimentEnabled("memorysnapshot") {
		b.vs.api.ms = &machineSnapshotRuntime{
			vm:          vm,
			kernelPath:  cfg.KernelPath,
			initrdPath:  b.initrd,
			rootfsPath:  cfg.RootfsPath,
			swapPath:    b.swapPath,
			commandLine: cfg.CommandLine,
			hostname:    b.setup.Hostname,
			user:        b.setup.User,
			homeDir:     b.setup.HomeDir,
			cwd:         b.setup.CWD,
			shares:      append([]string(nil), b.setup.Shares...),
			sshAgent:    b.setup.SSHAgent,
			cpus:        cfg.cpus(),
			memoryBytes: cfg.memoryBytes(),
		}
	}

	slog.Info("daemon ready, waiting for exec sessions")

	go func() {
		for state := range b.vm.StateChangedNotify() {
			switch state {
			case VMStateStarting, VMStateRunning:
				slog.Debug("vm state changed", "state", state)
			case VMStateStopped:
				slog.Warn("vm stopped while daemon was still running")
				b.vs.api.requestStop("vm stopped while daemon was still running", "state", state)
				return
			default:
				slog.Warn("vm entered unexpected state while daemon was still running", "state", state)
				b.vs.api.requestStop("vm entered unexpected state while daemon was still running", "state", state)
				return
			}
		}
		slog.Warn("vm state channel closed while daemon was still running")
		b.vs.api.requestStop("vm state channel closed while daemon was still running")
	}()

	// Block until idle (all execs finished) or stop requested.
	b.vs.api.WaitIdle()

	slog.Info("daemon shutting down")
	b.close(0)
	return nil
}

func runRestoredDaemon(cfg *Config) error {
	b, err := restoreDaemonVM(cfg)
	if err != nil {
		return err
	}
	if vm, ok := b.vm.(SnapshotCapableVirtualMachine); ok && experimentEnabled("memorysnapshot") {
		b.vs.api.ms = &machineSnapshotRuntime{
			vm:          vm,
			kernelPath:  cfg.KernelPath,
			initrdPath:  b.initrd,
			rootfsPath:  cfg.RootfsPath,
			swapPath:    b.swapPath,
			commandLine: cfg.CommandLine,
			hostname:    b.setup.Hostname,
			user:        b.setup.User,
			homeDir:     b.setup.HomeDir,
			cwd:         b.setup.CWD,
			shares:      append([]string(nil), b.setup.Shares...),
			sshAgent:    b.setup.SSHAgent,
			cpus:        cfg.cpus(),
			memoryBytes: cfg.memoryBytes(),
		}
	}

	slog.Info("daemon restored, waiting for exec sessions")

	go func() {
		for state := range b.vm.StateChangedNotify() {
			switch state {
			case VMStateStarting, VMStateRunning:
				slog.Debug("vm state changed", "state", state)
			case VMStateStopped:
				slog.Warn("vm stopped while daemon was still running")
				b.vs.api.requestStop("vm stopped while daemon was still running", "state", state)
				return
			default:
				slog.Warn("vm entered unexpected state while daemon was still running", "state", state)
				b.vs.api.requestStop("vm entered unexpected state while daemon was still running", "state", state)
				return
			}
		}
		slog.Warn("vm state channel closed while daemon was still running")
		b.vs.api.requestStop("vm state channel closed while daemon was still running")
	}()

	if err := RemoveMachineSnapshot(cfg.socketDir()); err != nil {
		slog.Warn("remove consumed machine snapshot failed", "error", err)
	}

	b.vs.api.WaitIdle()
	slog.Info("daemon shutting down")
	b.close(0)
	return nil
}

func restoreDaemonVM(cfg *Config) (*bootedVM, error) {
	initHostLoggingFromEnv()
	if cfg.Restore == nil {
		return nil, fmt.Errorf("restore config required")
	}
	if err := validatePaths(cfg); err != nil {
		return nil, err
	}

	lock, err := lockRootfs(cfg.RootfsPath)
	if err != nil {
		return nil, err
	}

	restore := cfg.Restore.Manifest
	setupMsg := &protocol.Setup{
		CWD:         restore.CWD,
		User:        restore.User,
		HomeDir:     restore.HomeDir,
		Hostname:    restore.Hostname,
		SSHAgent:    restore.SSHAgent,
		Shares:      append([]string(nil), restore.Shares...),
		ShareMethod: shareMethod(),
	}

	vm, err := buildVM(cfg, cfg.InitramfsPath, restore.CWD, restore.SwapPath, restore.HomeDir)
	if err != nil {
		lock.unlock()
		return nil, err
	}

	vs, err := setupVsock(vm.VsockDevice(), cfg.socketDir(), cfg.RootfsPath, setupMsg)
	if err != nil {
		lock.unlock()
		return nil, err
	}

	svm, ok := vm.(SnapshotCapableVirtualMachine)
	if !ok {
		vs.cleanup()
		lock.unlock()
		return nil, fmt.Errorf("vm restore unsupported")
	}
	if err := svm.RestoreMachineStateFromURL(cfg.Restore.Manifest.StatePath); err != nil {
		vs.cleanup()
		lock.unlock()
		return nil, fmt.Errorf("restore machine state: %w", err)
	}
	if err := svm.Resume(); err != nil {
		vs.cleanup()
		lock.unlock()
		return nil, fmt.Errorf("resume restored vm: %w", err)
	}

	return &bootedVM{
		vm:       vm,
		vs:       vs,
		cwd:      restore.CWD,
		cfg:      cfg,
		initrd:   cfg.InitramfsPath,
		swapPath: restore.SwapPath,
		setup:    setupMsg,
		lock:     lock,
	}, nil
}

// vsockState holds the vsock infrastructure created during VM setup.
type vsockState struct {
	ctrlConnCh <-chan net.Conn
	api        *apiServer
	cleanup    func()
}

func setupVsock(sock VsockDevice, logDir, rootfsPath string, setupMsg *protocol.Setup) (*vsockState, error) {
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

	// Additional 9P servers for CWD and shares when virtiofs is unavailable.
	var p9ShareListeners []net.Listener
	if setupMsg.ShareMethod == "9p" {
		// CWD share.
		cwdListener, err := sock.Listen(protocol.P9CWDPort)
		if err != nil {
			slog.Warn("vsock cwd 9p listen failed", "error", err)
		} else {
			start9PServerUnfiltered(cwdListener, setupMsg.CWD)
			p9ShareListeners = append(p9ShareListeners, cwdListener)
		}

		// Extra shares.
		for i, path := range setupMsg.Shares {
			shareListener, err := sock.Listen(protocol.P9ShareBasePort + uint32(i))
			if err != nil {
				slog.Warn("vsock share 9p listen failed", "path", path, "error", err)
				continue
			}
			start9PServerUnfiltered(shareListener, path)
			p9ShareListeners = append(p9ShareListeners, shareListener)
		}
	}

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
		for {
			conn, err := portFwdListener.Accept()
			if err != nil {
				return
			}
			go pf.run(conn)
		}
	}()

	api := newAPIServer(nil, setupMsg.User, rootfsPath)
	api.sock = sock
	api.pf = pf
	go func() {
		for {
			conn, err := statusListener.Accept()
			if err != nil {
				return
			}
			api.setStatusConn(conn)
		}
	}()
	go func() {
		for {
			conn, err := guestCtrlListener.Accept()
			if err != nil {
				return
			}
			api.setGuestCtrlConn(conn)
		}
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
		for _, l := range p9ShareListeners {
			l.Close()
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

// serialLogTail returns the last few lines of serial.log for error diagnostics.
func serialLogTail(dir string) string {
	data, err := os.ReadFile(filepath.Join(dir, "serial.log"))
	if err != nil || len(data) == 0 {
		return "serial.log: (not available)"
	}
	lines := strings.Split(strings.TrimSpace(string(data)), "\n")
	const maxLines = 20
	if len(lines) > maxLines {
		lines = lines[len(lines)-maxLines:]
	}
	return "serial.log:\n" + strings.Join(lines, "\n")
}
