package lnx

import (
	cryptoRand "crypto/rand"
	"encoding/gob"
	"fmt"
	"io"
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
	sockDir  string
	// ephCleanup removes the ephemeral temp dir (nil if not ephemeral).
	ephCleanup func()
	lock       *lockFile
}

// close shuts down the VM and releases all resources.
// If stopMode is "" (default) and the VM is not ephemeral, it attempts
// guest-side hibernate (Linux suspend-to-disk). The guest writes its state
// to swap and the kernel powers off. On next boot, the kernel auto-resumes.
// "shutdown" always does a full shutdown.
func (b *bootedVM) close(exitCode int, stopMode string) {
	hibernated := false
	if stopMode != "shutdown" && b.ephCleanup == nil && exitCode == 0 {
		hibernated = b.requestGuestHibernate()
	}

	if !hibernated {
		b.ctrlConn.Close()
		shutdownVM(b.vm, exitCode)
	}

	b.lock.unlock()
	b.vs.cleanup()
	if b.ephCleanup != nil {
		b.ephCleanup()
	}
}

// requestGuestHibernate sends a Hibernate message to the guest and waits
// for the VM to stop (kernel powers off after writing to swap). Returns
// true if hibernate succeeded, false if the host should fall back to shutdown.
func (b *bootedVM) requestGuestHibernate() bool {
	slog.Info("requesting guest hibernate")

	if err := b.ctrlEnc.Encode(protocol.Msg{Hibernate: &protocol.Hibernate{}}); err != nil {
		slog.Warn("failed to send hibernate request", "error", err)
		return false
	}

	// Read the guest's response. The gob decoder will also return an error
	// when the VM stops (connection closes), so this doubles as a VM-stop detector.
	type result struct {
		resp *protocol.HibernateResp
		err  error
	}
	respCh := make(chan result, 1)
	go func() {
		dec := gob.NewDecoder(b.ctrlConn)
		var msg protocol.Msg
		if err := dec.Decode(&msg); err != nil {
			respCh <- result{err: err}
			return
		}
		respCh <- result{resp: msg.HibernateResp}
		// After sending the response, the guest writes to /sys/power/state.
		// The kernel saves to swap then powers off, closing this connection.
		// Block until that happens (Decode returns error on close).
		dec.Decode(&msg)
		respCh <- result{err: fmt.Errorf("vm stopped")}
	}()

	timer := time.NewTimer(30 * time.Second)
	defer timer.Stop()

	// First wait for the guest's response.
	select {
	case r := <-respCh:
		if r.err != nil {
			slog.Warn("hibernate response failed", "error", r.err)
			return false
		}
		if r.resp != nil && r.resp.Error != "" {
			slog.Warn("guest reports hibernate not supported", "error", r.resp.Error)
			return false
		}
		slog.Info("guest acknowledged hibernate, waiting for VM to stop")
	case <-timer.C:
		slog.Warn("hibernate response timed out")
		return false
	}

	// Guest acknowledged — wait for the VM to power off. The second
	// Decode() in the goroutine above will return when the connection
	// closes (VM stopped after kernel hibernate).
	select {
	case <-respCh:
		slog.Info("VM stopped after hibernate")
		os.WriteFile(filepath.Join(b.sockDir, "hibernated"), []byte("1"), 0644)
		return true
	case <-time.After(120 * time.Second):
		slog.Warn("VM did not stop after hibernate within timeout")
		return false
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

	// Check for a hibernate marker early so ensureSwapFile can be skipped
	// when resuming (the swap file contains the kernel's hibernate image).
	sockDir := cfg.socketDir()
	hibernateMarker := filepath.Join(sockDir, "hibernated")
	resuming := ephCleanup == nil && fileExists(hibernateMarker)

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
	// Skip ensureSwapFile when resuming from hibernate — the swap file
	// contains the kernel's hibernate image and must not be overwritten,
	// even if the configured memory size differs from the checkpoint's.
	// Only skip if the swap file actually exists (a cloned instance may
	// have a stale hibernated marker without a swap file).
	if !resuming || !fileExists(swapPath) {
		if err := ensureSwapFile(swapPath, cfg.memoryBytes()); err != nil {
			lock.unlock()
			if ephCleanup != nil {
				ephCleanup()
			}
			return nil, fmt.Errorf("swap file: %w", err)
		}
	}

	hostname := cfg.Hostname
	if hostname == "" {
		hostname = "lnx"
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

	// Consume the marker — if the kernel can't resume it will cold boot.
	os.Remove(hibernateMarker)

	if resuming {
		slog.Info("booting VM (kernel will resume from hibernate)")
	}

	// Load or generate a stable MAC address for this instance.
	macAddr := loadOrGenerateMAC(sockDir)

	epoch := time.Now().Unix()

	vm, err := buildVM(cfg, initrdPath, cwd, swapPath, u.HomeDir, macAddr, epoch)
	if err != nil {
		lock.unlock()
		if ephCleanup != nil {
			ephCleanup()
		}
		return nil, err
	}

	vs, err := setupVsock(vm.VsockDevice(), sockDir, cfg.RootfsPath, setupMsg)
	if err != nil {
		lock.unlock()
		if ephCleanup != nil {
			ephCleanup()
		}
		return nil, err
	}

	bail := func(format string, args ...any) (*bootedVM, error) {
		vs.cleanup()
		vm.Stop()
		lock.unlock()
		if ephCleanup != nil {
			ephCleanup()
		}
		return nil, fmt.Errorf(format, args...)
	}

	if err := vm.Start(); err != nil {
		return bail("start vm: %w", err)
	}

	// Wait for the guest to connect on the control port, with a timeout.
	// On resume from hibernate, the restored guest init enters a reconnect
	// loop and dials this port. On cold boot, the fresh init connects after
	// setup. Either way, we wait for a connection here.
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
			case VMStateRunning, VMStateStarting, VMStatePaused:
				continue // expected transient states
			default:
				return bail("VM entered state %v during boot\n%s", state, serialLogTail(sockDir))
			}
		case <-bootTimer.C:
			return bail("guest did not connect within 30s\n%s", serialLogTail(sockDir))
		}
	}
	if ctrlConn == nil {
		return bail("control connection failed\n%s", serialLogTail(sockDir))
	}

	// Send the appropriate first message on the control connection.
	// On resume: Reconnect (guest already has setup state from the original boot).
	// On cold boot: Setup (guest needs environment config).
	enc := gob.NewEncoder(ctrlConn)
	var ctrlMsg protocol.Msg
	if resuming {
		ctrlMsg = protocol.Msg{Reconnect: &protocol.Reconnect{}}
	} else {
		ctrlMsg = protocol.Msg{Setup: setupMsg}
	}
	if err := enc.Encode(ctrlMsg); err != nil {
		ctrlConn.Close()
		return bail("send control message: %w", err)
	}

	return &bootedVM{
		vm:         vm,
		vs:         vs,
		ctrlConn:   ctrlConn,
		ctrlEnc:    enc,
		cwd:        cwd,
		sockDir:    sockDir,
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

	b.close(exitCode, "shutdown")
	return exitCode, nil
}

// RunDaemon boots a VM and runs it as a background daemon with no initial command.
// It blocks until all exec sessions have finished (idle) or Stop is requested
// via the API. Returns nil on clean shutdown.
//
// When a memory checkpoint or restore is requested, the daemon reboots the VM
// internally (hibernate → clone → resume) so the caller sees a brief pause
// rather than needing to restart the daemon.
func RunDaemon(cfg *Config) error {
	// Pending requests survive across reboot iterations. The HTTP handler
	// holds a reference to the Done channel; we signal it after the new VM
	// is up so the client gets a response only when the VM is ready again.
	var rebootCP *pendingCheckpointReq
	var rebootRestore *pendingRestoreReq

	for {
		b, err := bootVM(cfg)
		if err != nil {
			if rebootCP != nil {
				rebootCP.Done <- err
				close(rebootCP.Done)
			}
			if rebootRestore != nil {
				rebootRestore.Done <- err
				close(rebootRestore.Done)
			}
			return err
		}

		// If we rebooted after a checkpoint/restore, the VM is back.
		// Signal the waiting HTTP handler so it can respond to the client.
		if rebootCP != nil {
			slog.Info("VM resumed after memory checkpoint", "name", rebootCP.Name)
			rebootCP.Done <- nil
			close(rebootCP.Done)
			rebootCP = nil
		}
		if rebootRestore != nil {
			slog.Info("VM resumed after checkpoint restore", "name", rebootRestore.Name)
			rebootRestore.Done <- nil
			close(rebootRestore.Done)
			rebootRestore = nil
		}

		slog.Info("daemon ready, waiting for exec sessions")

		go func() {
			for state := range b.vm.StateChangedNotify() {
				switch state {
				case VMStateStarting, VMStateRunning:
					slog.Debug("vm state changed", "state", state)
				case VMStatePaused:
					// Expected during hibernate (Pause → SaveState → Stop).
					slog.Debug("vm paused (hibernate in progress)", "state", state)
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

		// Grab pending checkpoint/restore before close() tears down the API.
		b.vs.api.pendingCheckpointMu.Lock()
		pendingCP := b.vs.api.pendingCheckpoint
		b.vs.api.pendingCheckpoint = nil
		pendingRestore := b.vs.api.pendingRestore
		b.vs.api.pendingRestore = nil
		b.vs.api.pendingCheckpointMu.Unlock()

		slog.Info("daemon shutting down", "stopMode", b.vs.api.stopMode)

		// Save paths we need after close() releases the VM.
		rootfsPath := b.vs.api.rootfsPath
		sockDir := b.sockDir
		workDir := filepath.Dir(rootfsPath)
		if strings.HasPrefix(rootfsPath, "/dev/") {
			workDir = sockDir
		}

		b.close(0, b.vs.api.stopMode)

		// --- Memory checkpoint: clone rootfs+swap, then reboot ---
		if pendingCP != nil {
			cpDir := filepath.Join(workDir, "checkpoints")
			swapPath := filepath.Join(workDir, "swap.img")
			_, err := CreateMemoryCheckpoint(rootfsPath, swapPath, cpDir,
				pendingCP.Name, pendingCP.Description, pendingCP.Tags)
			if err != nil {
				slog.Error("memory checkpoint failed", "name", pendingCP.Name, "error", err)
				pendingCP.Done <- err
				close(pendingCP.Done)
				return err
			}
			slog.Info("memory checkpoint created, rebooting VM", "name", pendingCP.Name)
			rebootCP = pendingCP
			continue
		}

		// --- Checkpoint restore: replace files, then reboot ---
		if pendingRestore != nil {
			cpDir := filepath.Join(workDir, "checkpoints")
			swapPath := filepath.Join(workDir, "swap.img")
			err := RestoreMemoryCheckpoint(cpDir, pendingRestore.Name, rootfsPath, swapPath)
			if err != nil {
				slog.Error("checkpoint restore failed", "name", pendingRestore.Name, "error", err)
				pendingRestore.Done <- err
				close(pendingRestore.Done)
				return err
			}
			os.WriteFile(filepath.Join(sockDir, "hibernated"), []byte("1"), 0644)
			slog.Info("checkpoint restored, rebooting VM", "name", pendingRestore.Name)
			rebootRestore = pendingRestore
			continue
		}

		return nil
	}
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

func fileExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

// loadOrGenerateMAC returns a stable MAC address for an instance.
// On the first call for a given dir, it generates a random locally-administered
// MAC and persists it to mac.addr. Subsequent calls read the saved value.
// Returns "" if the dir is empty (ephemeral VMs get a random MAC each time).
func loadOrGenerateMAC(dir string) string {
	if dir == "" {
		return ""
	}
	path := filepath.Join(dir, "mac.addr")
	if data, err := os.ReadFile(path); err == nil {
		mac := strings.TrimSpace(string(data))
		if mac != "" {
			return mac
		}
	}
	// Generate a random locally-administered unicast MAC.
	// Format: x2:xx:xx:xx:xx:xx (bit 1 of first octet = locally administered,
	// bit 0 = unicast).
	b := make([]byte, 6)
	if _, err := io.ReadFull(cryptoRand.Reader, b); err != nil {
		return "" // fall back to VZ-generated random MAC
	}
	b[0] = (b[0] & 0xfe) | 0x02 // locally administered, unicast
	mac := fmt.Sprintf("%02x:%02x:%02x:%02x:%02x:%02x", b[0], b[1], b[2], b[3], b[4], b[5])
	os.WriteFile(path, []byte(mac+"\n"), 0644)
	return mac
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
