package lnx

import (
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

	vz "github.com/Code-Hex/vz/v3"
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

	initrdDir := filepath.Dir(cfg.KernelPath)
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

	if cfg.Interactive {
		fd := int(os.Stdin.Fd())
		if term.IsTerminal(fd) {
			oldState, err := term.MakeRaw(fd)
			if err != nil {
				return -1, fmt.Errorf("make raw: %w", err)
			}
			defer term.Restore(fd, oldState)
		}
	}

	vm, err := vz.NewVirtualMachine(vmConfig)
	if err != nil {
		return -1, fmt.Errorf("create vm: %w", err)
	}

	var rows, cols uint16
	if cfg.Interactive {
		fd := int(os.Stdin.Fd())
		if term.IsTerminal(fd) {
			w, h, err := term.GetSize(fd)
			if err == nil {
				rows = uint16(h)
				cols = uint16(w)
			}
		}
	}

	execMsg := &protocol.Exec{
		Args:    args,
		CWD:     cwd,
		Env:     buildGuestEnv(cfg.Env),
		PTY:     cfg.Interactive,
		User:    u.Username,
		UID:     uid,
		HomeDir: u.HomeDir,
		Rows:    rows,
		Cols:    cols,
	}

	exitCodeCh, cleanup, err := setupVsock(vm, filepath.Dir(cfg.RootfsPath), cfg.RootfsPath, execMsg)
	if err != nil {
		return -1, err
	}
	if err := vm.Start(); err != nil {
		cleanup()
		return -1, fmt.Errorf("start vm: %w", err)
	}

	return waitForVM(vm, exitCodeCh, cleanup)
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
		func(c *vz.VirtualMachineConfiguration) error { return attachShares(c, cwd, homeDir) },
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

func setupVsock(vm *vz.VirtualMachine, logDir, rootfsPath string, execMsg *protocol.Exec) (<-chan int, func(), error) {
	socketDevices := vm.SocketDevices()
	if len(socketDevices) == 0 {
		return nil, nil, fmt.Errorf("no vsock devices")
	}
	sock := socketDevices[0]

	logListener, err := sock.Listen(vsockLogPort)
	if err != nil {
		return nil, nil, fmt.Errorf("vsock log listen: %w", err)
	}
	waitLog := startLogReceiver(logListener, logDir)

	ctrlListener, err := sock.Listen(protocol.Port)
	if err != nil {
		logListener.Close()
		return nil, nil, fmt.Errorf("vsock ctrl listen: %w", err)
	}

	statusListener, err := sock.Listen(protocol.StatusPort)
	if err != nil {
		ctrlListener.Close()
		logListener.Close()
		return nil, nil, fmt.Errorf("vsock status listen: %w", err)
	}

	execListener, err := sock.Listen(protocol.ExecPort)
	if err != nil {
		statusListener.Close()
		ctrlListener.Close()
		logListener.Close()
		return nil, nil, fmt.Errorf("vsock exec listen: %w", err)
	}

	guestCtrlListener, err := sock.Listen(protocol.GuestControlPort)
	if err != nil {
		execListener.Close()
		statusListener.Close()
		ctrlListener.Close()
		logListener.Close()
		return nil, nil, fmt.Errorf("vsock guest ctrl listen: %w", err)
	}

	termListener, err := sock.Listen(protocol.TerminalPort)
	if err != nil {
		guestCtrlListener.Close()
		execListener.Close()
		statusListener.Close()
		ctrlListener.Close()
		logListener.Close()
		return nil, nil, fmt.Errorf("vsock terminal listen: %w", err)
	}

	// Terminal I/O: accept guest connection, splice os.Stdin/os.Stdout.
	go func() {
		conn, err := termListener.Accept()
		if err != nil {
			return
		}
		// stdin → guest
		go func() {
			io.Copy(conn, os.Stdin)
		}()
		// guest → stdout
		io.Copy(os.Stdout, conn)
	}()

	// Port forwarding: accept guest notification connection.
	portFwdListener, err := sock.Listen(protocol.PortForwardPort)
	if err != nil {
		termListener.Close()
		guestCtrlListener.Close()
		execListener.Close()
		statusListener.Close()
		ctrlListener.Close()
		logListener.Close()
		return nil, nil, fmt.Errorf("vsock port forward listen: %w", err)
	}

	pf := newPortForwarder(sock)
	go func() {
		conn, err := portFwdListener.Accept()
		if err != nil {
			return
		}
		pf.run(conn)
	}()

	// API server: accept guest connections, serve HTTP on unix socket.
	api := newAPIServer(execMsg.Args, execMsg.User, rootfsPath)
	api.pf = pf
	go func() {
		conn, err := statusListener.Accept()
		if err != nil {
			return
		}
		api.setStatusConn(conn)
	}()
	go func() {
		conn, err := execListener.Accept()
		if err != nil {
			return
		}
		api.setExecConn(conn)
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

	exitCodeCh := make(chan int, 1)
	go handleControlConn(ctrlListener, execMsg, exitCodeCh)

	cleanup := func() {
		pf.close()
		api.close()
		portFwdListener.Close()
		termListener.Close()
		guestCtrlListener.Close()
		execListener.Close()
		statusListener.Close()
		ctrlListener.Close()
		logListener.Close()
		waitLog()
	}

	return exitCodeCh, cleanup, nil
}

// handleControlConn accepts one guest connection, sends the Exec message,
// forwards host signals, and reads the Exit message.
func handleControlConn(listener interface {
	Accept() (net.Conn, error)
	Close() error
}, execMsg *protocol.Exec, exitCodeCh chan<- int) {
	conn, err := listener.Accept()
	if err != nil {
		slog.Debug("control accept failed", "error", err)
		exitCodeCh <- -1
		return
	}
	slog.Debug("control accepted")
	defer conn.Close()

	// Forward host signals and window size changes over the control connection.
	sigCh := make(chan os.Signal, 4)
	signal.Notify(sigCh, syscall.SIGTERM, syscall.SIGINT, syscall.SIGHUP, syscall.SIGWINCH)
	defer signal.Stop(sigCh)

	runControlConn(conn, execMsg, exitCodeCh, sigCh)
}

// runControlConn handles the gob protocol over an established connection.
// It sends the Exec message, forwards signals from sigCh, and reads
// messages until it receives Exit.
func runControlConn(conn net.Conn, execMsg *protocol.Exec, exitCodeCh chan<- int, sigCh <-chan os.Signal) {
	code := -1
	defer func() { exitCodeCh <- code }()

	enc := gob.NewEncoder(conn)
	dec := gob.NewDecoder(conn)

	if err := enc.Encode(protocol.Msg{Exec: execMsg}); err != nil {
		slog.Debug("control exec encode failed", "error", err)
		return
	}
	slog.Debug("control exec sent")

	// Forward signals in a goroutine. SIGWINCH is converted to a Resize
	// message with the current terminal dimensions instead of being forwarded
	// as a raw signal number. Two SIGINTs within 1 second force-exits.
	done := make(chan struct{})
	defer close(done)
	if sigCh != nil {
		go func() {
			var lastInt time.Time
			for {
				select {
				case sig := <-sigCh:
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
						code = 130
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
				case <-done:
					return
				}
			}
		}()
	}

	for {
		var msg protocol.Msg
		if err := dec.Decode(&msg); err != nil {
			slog.Debug("control decode failed", "error", err)
			return
		}
		if msg.Exit != nil {
			code = msg.Exit.Code
			slog.Debug("control exit received", "code", code)
			_ = enc.Encode(protocol.Msg{Ack: &protocol.Ack{}})
			slog.Debug("control ack sent")
			return
		}
	}
}

func waitForVM(vm *vz.VirtualMachine, exitCodeCh <-chan int, cleanup func()) (int, error) {
	defer cleanup()

	// Wait for either the exit code (guest finished) or a VM state change.
	stateCh := vm.StateChangedNotify()
	for {
		select {
		case code := <-exitCodeCh:
			if code == 130 {
				// Force quit (double Ctrl-C): stop VM immediately.
				vm.Stop()
			} else {
				// Guest sent Exit, host sent Ack, we have the code.
				// Request graceful stop; the guest is already calling
				// poweroff. If it doesn't stop quickly, force-stop.
				vm.RequestStop()
				select {
				case <-time.After(3 * time.Second):
					vm.Stop()
				case state := <-stateCh:
					if state != vz.VirtualMachineStateStopped {
						vm.Stop()
					}
				}
			}
			return code, nil
		case state := <-stateCh:
			switch state {
			case vz.VirtualMachineStateStopped:
				// VM stopped before we got an exit code (crash/freeze).
				cleanup()
				select {
				case code := <-exitCodeCh:
					return code, nil
				default:
					return -1, nil
				}
			case vz.VirtualMachineStateError:
				cleanup()
				return -1, fmt.Errorf("vm entered error state")
			}
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
		levelName := strings.ToLower(os.Getenv("LNX_LOG"))
		if levelName == "" {
			return
		}

		level := slog.LevelInfo
		switch levelName {
		case "debug":
			level = slog.LevelDebug
		case "warn":
			level = slog.LevelWarn
		case "error":
			level = slog.LevelError
		}

		slog.SetDefault(slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: level})))
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
