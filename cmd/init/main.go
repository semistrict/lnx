//go:build linux

package main

import (
	"encoding/gob"
	"fmt"
	"io"
	"log/slog"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"sync"
	"syscall"

	"github.com/mdlayher/vsock"
	"github.com/semistrict/lnx/internal/protocol"
	"golang.org/x/sys/unix"
)

const (
	vsockHostCID = 2
	vsockLogPort = 1025
)

func main() {
	exitCode := 0
	if err := run(&exitCode); err != nil {
		slog.Error("init failed", "error", err)
		exitCode = 127
	}
	sendExitAndWait(exitCode)
	poweroff()
}

// ctrlEnc/ctrlDec are the gob encoder/decoder for the control connection.
var (
	ctrlConn   *vsock.Conn
	ctrlEnc    *gob.Encoder
	ctrlDec    *gob.Decoder
	ctrlAckCh  chan struct{}
	ctrlAckMu  sync.Once
	ctrlProc   *os.Process
	ctrlProcMu sync.RWMutex
	ctrlPTY    *os.File
	ctrlPTYMu  sync.RWMutex
)

func initLogging() {
	level := slog.LevelInfo
	switch strings.ToLower(os.Getenv("LNX_LOG")) {
	case "debug":
		level = slog.LevelDebug
	case "warn":
		level = slog.LevelWarn
	case "error":
		level = slog.LevelError
	}

	conn, err := vsock.Dial(vsockHostCID, vsockLogPort, nil)
	if err != nil {
		slog.SetDefault(slog.New(slog.NewJSONHandler(os.Stderr, &slog.HandlerOptions{Level: level})))
		return
	}
	slog.SetDefault(slog.New(slog.NewJSONHandler(conn, &slog.HandlerOptions{Level: level})))
}

func sendExitAndWait(code int) {
	if ctrlEnc == nil {
		return
	}
	_ = ctrlEnc.Encode(protocol.Msg{Exit: &protocol.Exit{Code: code}})
	if ctrlAckCh != nil {
		<-ctrlAckCh
	}
	if ctrlConn != nil {
		_ = ctrlConn.Close()
	}
}

func run(exitCode *int) error {
	if err := mountInitialFS(); err != nil {
		return err
	}

	initLogging()

	// Parse epoch from kernel cmdline (needed before vsock for clock).
	parseEpoch()

	// Connect to the host control channel.
	conn, err := vsock.Dial(vsockHostCID, protocol.Port, nil)
	if err != nil {
		return fmt.Errorf("vsock dial control: %w", err)
	}
	ctrlConn = conn

	ctrlEnc = gob.NewEncoder(ctrlConn)
	ctrlDec = gob.NewDecoder(ctrlConn)
	ctrlAckCh = make(chan struct{})
	ctrlAckMu = sync.Once{}

	// Read the Exec message from the host.
	var msg protocol.Msg
	if err := ctrlDec.Decode(&msg); err != nil {
		return fmt.Errorf("decode exec msg: %w", err)
	}
	if msg.Exec == nil {
		return fmt.Errorf("expected Exec message, got %+v", msg)
	}
	execMsg := msg.Exec
	startControlReader()

	if err := mountRootfs(); err != nil {
		return err
	}
	// Mount home read-only first, then CWD read-write on top.
	// CWD overlays the home mount if it's a subdirectory.
	if execMsg.HomeDir != "" {
		if err := mountHome(execMsg.HomeDir); err != nil {
			return err
		}
	}
	if execMsg.CWD != "" {
		if err := mountCWD(execMsg.CWD); err != nil {
			return err
		}
	}
	if err := mountInNewRoot(); err != nil {
		return err
	}
	if err := pivotRoot(); err != nil {
		return err
	}

	// Online resize (no-op if already full size).
	if out, err := exec.Command("/sbin/resize2fs", "/dev/vda").CombinedOutput(); err != nil {
		slog.Warn("resize2fs failed", "error", err, "output", string(out))
	} else {
		slog.Info("resize2fs", "output", strings.TrimSpace(string(out)))
	}

	// Enable swap on /dev/vdb.
	if _, err := os.Stat("/dev/vdb"); err == nil {
		if out, err := exec.Command("/sbin/mkswap", "/dev/vdb").CombinedOutput(); err != nil {
			slog.Warn("mkswap failed", "error", err, "output", string(out))
		} else if out, err := exec.Command("/sbin/swapon", "/dev/vdb").CombinedOutput(); err != nil {
			slog.Warn("swapon failed", "error", err, "output", string(out))
		} else {
			slog.Info("swap enabled", "device", "/dev/vdb")
		}
	}

	syscall.Sethostname([]byte("lnx"))

	os.Setenv("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
	os.Setenv("TERM", "xterm-256color")
	os.Setenv("LANG", "C.UTF-8")

	// Set up the guest user matching the host user.
	if execMsg.User != "" && execMsg.UID > 0 {
		setupUser(execMsg.User, execMsg.UID)
		os.Setenv("HOME", "/home/"+execMsg.User)
		os.Setenv("USER", execMsg.User)
		os.Setenv("LOGNAME", execMsg.User)
	} else {
		os.Setenv("HOME", "/root")
	}

	// Apply environment from host.
	for _, kv := range execMsg.Env {
		if k, v, ok := strings.Cut(kv, "="); ok {
			os.Setenv(k, v)
		}
	}

	configureNetwork()
	writeResolvConf()
	installXdgOpen()
	startStatusServer()
	startExecServer()
	startGuestControlServer()
	startPortForwarder()

	slog.Info("running command", "args", execMsg.Args, "cwd", execMsg.CWD, "pty", execMsg.PTY, "user", execMsg.User, "uid", execMsg.UID)

	if execMsg.PTY {
		return runWithPTY(execMsg.Args, execMsg.CWD, execMsg.UID, execMsg.Rows, execMsg.Cols, exitCode)
	}
	return runDirect(execMsg.Args, execMsg.CWD, execMsg.UID, exitCode)
}

func parseEpoch() {
	data, err := os.ReadFile("/proc/cmdline")
	if err != nil {
		return
	}
	for _, param := range strings.Fields(string(data)) {
		if v, ok := strings.CutPrefix(param, "lnx.epoch="); ok {
			setClockFromEpoch(v)
		}
	}
}

func setClockFromEpoch(epochStr string) {
	epoch, err := strconv.ParseInt(epochStr, 10, 64)
	if err != nil {
		return
	}
	tv := syscall.Timeval{Sec: epoch}
	if err := syscall.Settimeofday(&tv); err != nil {
		slog.Warn("settimeofday failed", "error", err)
	}
}

// startControlReader is the sole decoder for the control connection.
// It forwards signals to the current child process and waits for the host ack.
func startControlReader() {
	go func() {
		for {
			var msg protocol.Msg
			if err := ctrlDec.Decode(&msg); err != nil {
				signalControlAck()
				return
			}
			if msg.Signal != nil {
				ctrlProcMu.RLock()
				proc := ctrlProc
				ctrlProcMu.RUnlock()
				if proc != nil {
					_ = proc.Signal(syscall.Signal(msg.Signal.Sig))
				}
			}
			if msg.Resize != nil {
				ctrlPTYMu.RLock()
				f := ctrlPTY
				ctrlPTYMu.RUnlock()
				if f != nil {
					_ = unix.IoctlSetWinsize(int(f.Fd()), unix.TIOCSWINSZ, &unix.Winsize{
						Row: msg.Resize.Rows,
						Col: msg.Resize.Cols,
					})
				}
			}
			if msg.Ack != nil {
				signalControlAck()
				return
			}
		}
	}()
}

func signalControlAck() {
	ctrlAckMu.Do(func() {
		if ctrlAckCh != nil {
			close(ctrlAckCh)
		}
	})
}

func setControlProcess(proc *os.Process) {
	ctrlProcMu.Lock()
	defer ctrlProcMu.Unlock()
	ctrlProc = proc
}

func setControlPTY(f *os.File) {
	ctrlPTYMu.Lock()
	defer ctrlPTYMu.Unlock()
	ctrlPTY = f
}

// setupUser creates a guest user matching the host user.
func setupUser(username string, uid int) {
	gid := uid
	home := "/home/" + username

	// Append to /etc/passwd, /etc/shadow, and /etc/group.
	appendFile("/etc/passwd", fmt.Sprintf("%s:x:%d:%d::%s:/bin/bash\n", username, uid, gid, home))
	appendFile("/etc/shadow", fmt.Sprintf("%s:!::0:99999:7:::\n", username))
	appendFile("/etc/group", fmt.Sprintf("%s:x:%d:\n", username, gid))

	// Create home directory.
	os.MkdirAll(home, 0755)
	os.Chown(home, uid, gid)

	// Passwordless sudo.
	os.MkdirAll("/etc/sudoers.d", 0755)
	os.WriteFile("/etc/sudoers.d/lnx", []byte(username+" ALL=(ALL) NOPASSWD: ALL\n"), 0440)
}

func appendFile(path, line string) {
	f, err := os.OpenFile(path, os.O_APPEND|os.O_WRONLY|os.O_CREATE, 0644)
	if err != nil {
		slog.Warn("append file failed", "path", path, "error", err)
		return
	}
	f.WriteString(line)
	f.Close()
}

func runDirect(args []string, cwdPath string, uid int, exitCode *int) error {
	termConn, err := vsock.Dial(vsockHostCID, protocol.TerminalPort, nil)
	if err != nil {
		return fmt.Errorf("vsock dial terminal: %w", err)
	}
	defer termConn.Close()

	cmd := exec.Command(args[0], args[1:]...)
	cmd.Stdout = termConn
	cmd.Stderr = termConn
	cmd.Env = os.Environ()
	if cwdPath != "" {
		cmd.Dir = cwdPath
	}
	if uid > 0 {
		cmd.SysProcAttr = &syscall.SysProcAttr{
			Credential: &syscall.Credential{
				Uid: uint32(uid),
				Gid: uint32(uid),
			},
		}
	}

	// Use StdinPipe so cmd.Wait() doesn't block on the vsock read.
	// The vsock terminal never closes (host stdin stays open), so
	// passing it directly as cmd.Stdin would hang cmd.Wait() forever.
	stdinPipe, err := cmd.StdinPipe()
	if err != nil {
		return fmt.Errorf("stdin pipe: %w", err)
	}

	if err := cmd.Start(); err != nil {
		slog.Error("exec failed", "error", err)
		*exitCode = 127
		return nil
	}
	setControlProcess(cmd.Process)
	defer setControlProcess(nil)

	// Copy terminal vsock → stdin pipe in background.
	go func() {
		io.Copy(stdinPipe, termConn)
		stdinPipe.Close()
	}()

	if err := cmd.Wait(); err != nil {
		if exitErr, ok := err.(*exec.ExitError); ok {
			*exitCode = exitErr.ExitCode()
		} else {
			slog.Error("wait failed", "error", err)
			*exitCode = 127
		}
	}
	slog.Info("command finished", "exit_code", *exitCode)
	return nil
}

// installXdgOpen writes a shim that forwards xdg-open calls to the host
// macOS browser via the guest control socket.
func installXdgOpen() {
	script := `#!/bin/sh
curl -sf --unix-socket /var/run/lnx/control.sock \
  -X POST -H "Content-Type: application/json" \
  -d "{\"url\":\"$1\"}" \
  http://localhost/open >/dev/null 2>&1
`
	if err := os.WriteFile("/usr/local/bin/xdg-open", []byte(script), 0755); err != nil {
		slog.Warn("failed to install xdg-open shim", "error", err)
	}
}

func poweroff() {
	syscall.Sync()
	unix.Reboot(unix.LINUX_REBOOT_CMD_POWER_OFF)
}
