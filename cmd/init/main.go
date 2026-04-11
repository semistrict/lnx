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
	"time"

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
	ctrlEnc   *gob.Encoder
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
	ctrlEnc = gob.NewEncoder(conn)
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

	installSystemctlShim()
	installSystemdCatShim()
	configureNetwork()
	installBashDefaults()
	installXdgOpen()
	startEnabledServices()
	startStatusServer()
	startExecServer()
	startGuestControlServer()
	startPortForwarder()

	slog.Info("guest ready", "user", setup.User, "uid", setup.UID)

	// Block until the host closes the control connection, then try to
	// reconnect (the host may have hibernated and a new daemon restored us).
	for {
		<-ctrlDone
		if !reconnectToHost() {
			break
		}
		slog.Info("reconnected to host after restore")
	}
	return nil
}

// reconnectToHost attempts to re-establish the control connection to a new
// host daemon after a hibernate/restore cycle. Returns true on success.
//
// On restore the kernel resumes exactly where it was paused. Outbound vsock
// connections to the old host are dead, but in-kernel vsock listeners (exec,
// interactive, port-fwd-data) survive. We re-dial the control port and all
// outbound service connections.
func reconnectToHost() bool {
	var conn *vsock.Conn
	var err error

	// The new host daemon sets up vsock listeners before resuming the VM,
	// so the port should be available quickly. Retry for up to 10 seconds
	// to handle any scheduling delay.
	for i := 0; i < 100; i++ {
		conn, err = vsock.Dial(vsockHostCID, protocol.Port, nil)
		if err == nil {
			break
		}
		time.Sleep(100 * time.Millisecond)
	}
	if conn == nil {
		slog.Info("reconnect failed, shutting down", "error", err)
		return false
	}

	dec := gob.NewDecoder(conn)
	var msg protocol.Msg
	if err := dec.Decode(&msg); err != nil {
		slog.Warn("reconnect decode failed", "error", err)
		conn.Close()
		return false
	}
	if msg.Reconnect == nil {
		slog.Warn("expected Reconnect message after restore", "msg", fmt.Sprintf("%+v", msg))
		conn.Close()
		return false
	}

	// Reset global control state.
	ctrlConn = conn
	ctrlDec = dec
	ctrlEnc = gob.NewEncoder(conn)
	ctrlDone = make(chan struct{})
	go controlReader()

	// Re-establish outbound service connections. These services run in
	// goroutines that exited when their old connections broke, so we
	// restart them.
	reconnectServices()
	return true
}

// reconnectServices re-dials all outbound vsock service connections
// that broke during hibernate. Called after the control connection is
// re-established.
func reconnectServices() {
	// Re-init logging over vsock (the old connection is dead).
	reconnectLogging()

	// Exec server: the old vsock listeners are dead after hibernate.
	startExecServer()

	// Status server: the old goroutine exited when its connection broke.
	startStatusServer()

	// Guest control: same pattern.
	startGuestControlServer()

	// Port forwarder: the scan goroutine exited when its connection broke.
	startPortForwarder()
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

// controlReader reads Signal, Resize, and Hibernate messages from the host.
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
		if msg.Hibernate != nil {
			go handleHibernate()
		}
	}
}

// handleHibernate initiates Linux suspend-to-disk. On success the kernel
// writes its state to swap and powers off the VM. On failure (kernel
// doesn't support hibernate, etc.) it sends an error back to the host.
func handleHibernate() {
	slog.Info("hibernate requested")

	// Check if the kernel supports hibernate.
	states, err := os.ReadFile("/sys/power/state")
	if err != nil {
		sendHibernateError("read /sys/power/state: %v", err)
		return
	}
	supported := false
	for _, s := range strings.Fields(string(states)) {
		if s == "disk" {
			supported = true
			break
		}
	}
	if !supported {
		sendHibernateError("kernel does not support disk hibernate (available: %s)", strings.TrimSpace(string(states)))
		return
	}

	// Unmount shared filesystems (9P/virtiofs) before hibernate so the
	// kernel doesn't try to freeze their devices. They're remounted on resume.
	unmounted := unmountSharedFS()

	syscall.Sync()

	// Tell the host we're about to hibernate.
	ctrlEnc.Encode(protocol.Msg{HibernateResp: &protocol.HibernateResp{}})

	slog.Info("initiating suspend-to-disk", "unmounted", len(unmounted))
	if err := os.WriteFile("/sys/power/state", []byte("disk"), 0644); err != nil {
		slog.Error("hibernate failed", "error", err)
		remountSharedFS(unmounted)
		return
	}

	// If we reach here, the kernel resumed from hibernate.
	slog.Info("resumed from hibernate, remounting shared filesystems")
	remountSharedFS(unmounted)
}

func sendHibernateError(format string, args ...any) {
	errMsg := fmt.Sprintf(format, args...)
	slog.Warn("hibernate not supported", "error", errMsg)
	ctrlEnc.Encode(protocol.Msg{HibernateResp: &protocol.HibernateResp{Error: errMsg}})
}

// sharedFSMount holds info needed to remount a shared filesystem after hibernate.
type sharedFSMount struct {
	device string // e.g. "cwd", "share0", "home"
	target string // e.g. "/Users/ramon/src/lnx"
	fstype string // e.g. "virtiofs", "9p"
}

// unmountSharedFS finds and lazy-unmounts all virtiofs and 9p mounts.
// Returns the info needed to remount them after hibernate resume.
// Uses MNT_DETACH because mounts may be busy (processes using them as CWD).
func unmountSharedFS() []sharedFSMount {
	data, err := os.ReadFile("/proc/mounts")
	if err != nil {
		return nil
	}
	var mounts []sharedFSMount
	for _, line := range strings.Split(string(data), "\n") {
		fields := strings.Fields(line)
		if len(fields) < 3 {
			continue
		}
		if fields[2] != "virtiofs" && fields[2] != "9p" {
			continue
		}
		mounts = append(mounts, sharedFSMount{
			device: fields[0],
			target: fields[1],
			fstype: fields[2],
		})
	}
	// Unmount in reverse order (nested mounts first).
	for i := len(mounts) - 1; i >= 0; i-- {
		m := mounts[i]
		if err := syscall.Unmount(m.target, syscall.MNT_DETACH); err != nil {
			slog.Warn("unmount shared fs failed", "target", m.target, "error", err)
		} else {
			slog.Debug("unmounted shared fs", "device", m.device, "target", m.target, "fstype", m.fstype)
		}
	}
	return mounts
}

// remountSharedFS remounts shared filesystems that were unmounted for hibernate.
func remountSharedFS(mounts []sharedFSMount) {
	for _, m := range mounts {
		os.MkdirAll(m.target, 0755)
		if err := syscall.Mount(m.device, m.target, m.fstype, 0, ""); err != nil {
			slog.Warn("remount shared fs failed", "device", m.device, "target", m.target, "error", err)
		} else {
			slog.Debug("remounted shared fs", "device", m.device, "target", m.target)
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

var logLevel slog.Level

func initLogging() {
	logLevel = slog.LevelInfo
	switch strings.ToLower(os.Getenv("LNX_LOG")) {
	case "debug":
		logLevel = slog.LevelDebug
	case "warn":
		logLevel = slog.LevelWarn
	case "error":
		logLevel = slog.LevelError
	}

	conn, err := vsock.Dial(vsockHostCID, vsockLogPort, nil)
	if err != nil {
		slog.SetDefault(slog.New(slog.NewJSONHandler(os.Stderr, &slog.HandlerOptions{Level: logLevel})))
		return
	}
	slog.SetDefault(slog.New(slog.NewJSONHandler(conn, &slog.HandlerOptions{Level: logLevel})))
}

// reconnectLogging re-dials the host log port and resets the default logger.
// Called after a hibernate/restore cycle when the old log connection is dead.
func reconnectLogging() {
	conn, err := vsock.Dial(vsockHostCID, vsockLogPort, nil)
	if err != nil {
		slog.SetDefault(slog.New(slog.NewJSONHandler(os.Stderr, &slog.HandlerOptions{Level: logLevel})))
		return
	}
	slog.SetDefault(slog.New(slog.NewJSONHandler(conn, &slog.HandlerOptions{Level: logLevel})))
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
