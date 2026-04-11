//go:build linux

package main

import (
	"bytes"
	"encoding/gob"
	"fmt"
	"io"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
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
	// Busybox-style dispatch: if invoked as "systemctl", run that instead.
	base := filepath.Base(os.Args[0])
	if base == "systemctl" {
		os.Exit(runSystemctl(os.Args[1:]))
	}
	if base == "systemd-cat" {
		initLogging()
		os.Exit(runSystemdCat(os.Args[1:]))
	}

	if err := run(); err != nil {
		slog.Error("init failed", "error", err)
	}
	poweroff()
}

// ctrlConn is the control connection to the host.
// It carries Setup, Signal, and Resize messages.
var (
	ctrlConn  *vsock.Conn
	ctrlDec   *gob.Decoder
	ctrlDone  chan struct{} // closed when control connection drops
	ctrlProc  *os.Process
	ctrlMu    sync.RWMutex
	ctrlPTY   *os.File
	ctrlPTYMu sync.RWMutex

	setupUID int    // UID from the host Setup message
	setupCWD string // CWD from the host Setup message
)

func run() error {
	if err := mountInitialFS(); err != nil {
		return err
	}

	initLogging()
	parseEpoch()

	// Connect to the host control channel.
	conn, err := vsock.Dial(vsockHostCID, protocol.Port, nil)
	if err != nil {
		return fmt.Errorf("vsock dial control: %w", err)
	}
	ctrlConn = conn
	ctrlDec = gob.NewDecoder(conn)
	ctrlDone = make(chan struct{})

	// Read the Setup message from the host.
	var msg protocol.Msg
	if err := ctrlDec.Decode(&msg); err != nil {
		return fmt.Errorf("decode setup msg: %w", err)
	}
	if msg.Setup == nil {
		return fmt.Errorf("expected Setup message, got %+v", msg)
	}
	setup := msg.Setup

	// Start reading signals/resize from control connection.
	go controlReader()

	if err := mountRootfs(); err != nil {
		return err
	}
	if setup.HomeDir != "" {
		if err := mountHome(setup.HomeDir); err != nil {
			slog.Warn("home 9p mount failed, continuing without it", "error", err)
		}
	}
	if setup.CWD != "" {
		if err := mountCWD(setup.CWD, setup.ShareMethod); err != nil {
			return err
		}
	}
	for i, path := range setup.Shares {
		if err := mountShare(path, fmt.Sprintf("share%d", i), setup.ShareMethod, i); err != nil {
			slog.Warn("share mount failed", "path", path, "error", err)
		}
	}
	if err := mountInNewRoot(); err != nil {
		return err
	}
	if err := pivotRoot(); err != nil {
		return err
	}

	mountCgroups()
	writeNestedDrivesMapping(setup)

	if out, err := exec.Command("/sbin/resize2fs", "/dev/vda").CombinedOutput(); err != nil {
		slog.Warn("resize2fs failed", "error", err, "output", string(out))
	} else {
		slog.Info("resize2fs", "output", strings.TrimSpace(string(out)))
	}

	if _, err := os.Stat("/dev/vdb"); err == nil {
		if out, err := exec.Command("/sbin/mkswap", "/dev/vdb").CombinedOutput(); err != nil {
			slog.Warn("mkswap failed", "error", err, "output", string(out))
		} else if out, err := exec.Command("/sbin/swapon", "/dev/vdb").CombinedOutput(); err != nil {
			slog.Warn("swapon failed", "error", err, "output", string(out))
		} else {
			slog.Info("swap enabled", "device", "/dev/vdb")
		}
	}

	hostname := setup.Hostname
	if hostname == "" {
		hostname = "lnx"
	}
	syscall.Sethostname([]byte(hostname))

	os.Setenv("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
	os.Setenv("TERM", "xterm-256color")
	os.Setenv("LANG", "C.UTF-8")
	os.Setenv("BROWSER", "xdg-open")

	setupUID = setup.UID
	setupCWD = setup.CWD
	if setup.User != "" && setup.UID > 0 {
		setupUser(setup.User, setup.UID)
		os.Setenv("HOME", "/home/"+setup.User)
		os.Setenv("USER", setup.User)
		os.Setenv("LOGNAME", setup.User)
	} else {
		os.Setenv("HOME", "/root")
	}

	for _, kv := range setup.Env {
		if k, v, ok := strings.Cut(kv, "="); ok {
			os.Setenv(k, v)
		}
	}

	if setup.SSHAgent {
		startSSHAgentForward()
	}

	mountCRIUDevice()

	// Bring up loopback before CRIU restore — needed for TCP repair
	// of loopback connections in the restored process tree.
	if out, err := exec.Command("/sbin/ip", "link", "set", "lo", "up").CombinedOutput(); err != nil {
		slog.Warn("loopback up failed", "error", err, "output", string(out))
	}

	// Auto-restore CRIU images immediately after mounting the CRIU device,
	// before ANY commands that fork processes (like ip, resize2fs). CRIU
	// restores processes to their original PIDs, which must not be taken.
	criuAutoRestore()

	installSystemctlShim()
	installSystemdCatShim()
	configureNetwork()

	installBashDefaults()
	installXdgOpen()
	installForkRoleHelper()
	startEnabledServices()
	startStatusServer()
	startExecServer()
	startGuestControlServer()
	startPortForwarder()

	slog.Info("guest ready", "user", setup.User, "uid", setup.UID)

	// Block until the host closes the control connection.
	<-ctrlDone
	return nil
}

func runSystemdCat(args []string) int {
	var cmdArgs []string
	identifier := "systemd-cat"
	priority := "6"
	for i := 0; i < len(args); i++ {
		arg := args[i]
		if arg == "--" {
			cmdArgs = args[i+1:]
			break
		}
		if strings.HasPrefix(arg, "-") {
			switch arg {
			case "-t", "--identifier":
				if i+1 < len(args) {
					identifier = args[i+1]
					i++
				}
			case "-p", "--priority":
				if i+1 < len(args) {
					priority = args[i+1]
					i++
				}
			case "--level-prefix":
				if i+1 < len(args) {
					i++
				}
			}
			continue
		}
		cmdArgs = args[i:]
		break
	}

	if len(cmdArgs) == 0 {
		data, _ := io.ReadAll(os.Stdin)
		logSystemdCatOutput(identifier, priority, data)
		return 0
	}

	cmd := exec.Command(cmdArgs[0], cmdArgs[1:]...)
	cmd.Stdin = os.Stdin
	var out bytes.Buffer
	cmd.Stdout = &out
	cmd.Stderr = &out
	if err := cmd.Run(); err != nil {
		logSystemdCatOutput(identifier, priority, out.Bytes())
		if ee, ok := err.(*exec.ExitError); ok {
			return ee.ExitCode()
		}
		fmt.Fprintln(os.Stderr, err)
		return 1
	}
	logSystemdCatOutput(identifier, priority, out.Bytes())
	return 0
}

func logSystemdCatOutput(identifier, priority string, data []byte) {
	text := strings.TrimSpace(string(data))
	if text == "" {
		return
	}
	attrs := []any{"identifier", identifier, "priority", priority}
	for _, line := range strings.Split(text, "\n") {
		line = strings.TrimSpace(line)
		if line == "" {
			continue
		}
		switch priority {
		case "0", "1", "2", "3":
			slog.Error(line, attrs...)
		case "4":
			slog.Warn(line, attrs...)
		default:
			slog.Info(line, attrs...)
		}
	}
}

// controlReader reads Signal and Resize messages from the host.
// When the connection closes, it signals ctrlDone.
func controlReader() {
	defer close(ctrlDone)
	for {
		var msg protocol.Msg
		if err := ctrlDec.Decode(&msg); err != nil {
			return
		}
		if msg.Signal != nil {
			ctrlMu.RLock()
			proc := ctrlProc
			ctrlMu.RUnlock()
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
	}
}

func setControlProcess(proc *os.Process) {
	ctrlMu.Lock()
	defer ctrlMu.Unlock()
	ctrlProc = proc
}

func setControlPTY(f *os.File) {
	ctrlPTYMu.Lock()
	defer ctrlPTYMu.Unlock()
	ctrlPTY = f
}

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

func setupUser(username string, uid int) {
	// Skip if user was already created in a prior boot.
	if data, err := os.ReadFile("/etc/passwd"); err == nil {
		if strings.Contains(string(data), username+":") {
			addUserToGroups(username)
			return
		}
	}

	gid := uid
	home := "/home/" + username

	appendFile("/etc/passwd", fmt.Sprintf("%s:x:%d:%d::%s:/bin/bash\n", username, uid, gid, home))
	appendFile("/etc/shadow", fmt.Sprintf("%s:!::0:99999:7:::\n", username))
	appendFile("/etc/group", fmt.Sprintf("%s:x:%d:\n", username, gid))

	os.MkdirAll(home, 0755)
	os.Chown(home, uid, gid)

	os.MkdirAll("/etc/sudoers.d", 0755)
	os.WriteFile("/etc/sudoers.d/lnx", []byte(username+" ALL=(ALL) NOPASSWD: ALL\n"), 0440)

	addUserToGroups(username)
}

// addUserToGroups adds the user to well-known system groups (docker, etc.)
// if they exist on the rootfs. Runs on every boot since groups may be
// added by package installs between boots.
func addUserToGroups(username string) {
	groups := []string{"docker", "sudo", "adm"}
	data, err := os.ReadFile("/etc/group")
	if err != nil {
		return
	}
	lines := strings.Split(string(data), "\n")
	changed := false
	for i, line := range lines {
		parts := strings.SplitN(line, ":", 4)
		if len(parts) != 4 {
			continue
		}
		groupName := parts[0]
		members := parts[3]
		found := false
		for _, g := range groups {
			if groupName == g {
				found = true
				break
			}
		}
		if !found {
			continue
		}
		// Check if user is already a member.
		memberList := strings.Split(members, ",")
		alreadyMember := false
		for _, m := range memberList {
			if m == username {
				alreadyMember = true
				break
			}
		}
		if alreadyMember {
			continue
		}
		if members == "" {
			parts[3] = username
		} else {
			parts[3] = members + "," + username
		}
		lines[i] = strings.Join(parts, ":")
		changed = true
	}
	if changed {
		os.WriteFile("/etc/group", []byte(strings.Join(lines, "\n")), 0644)
	}
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

// installBashDefaults ensures bash has color support and standard aliases
// even when the user's home dir is a read-only 9P mount without .bashrc.
func installBashDefaults() {
	script := `# lnx: source skeleton bashrc for color support
if [ -n "$BASH_VERSION" ] && [ -f /etc/skel/.bashrc ]; then
    . /etc/skel/.bashrc
fi
`
	os.MkdirAll("/etc/profile.d", 0755)
	os.WriteFile("/etc/profile.d/lnx-bashrc.sh", []byte(script), 0644)
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
