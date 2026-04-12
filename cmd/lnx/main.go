package main

import (
	"errors"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"time"

	"github.com/spf13/cobra"
	"golang.org/x/term"
)

var doCheckpoint bool
var doEphemeral bool
var doSSHAgent bool

// instanceName is the resolved instance name. Set from --instance flag or LNX_INSTANCE env.
var instanceName = "default"

// instanceFlag tracks whether --instance was explicitly set (flag or env var).
var instanceFlag bool

var rootCmd = &cobra.Command{
	Use:           "lnx [flags] [command [args...]]",
	Short:         "Run commands in a lightweight Linux VM",
	SilenceUsage:  true,
	SilenceErrors: true,
	Args:          cobra.ArbitraryArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		if len(args) == 0 {
			args = []string{"bash", "-l"}
		}
		exitCode, err := runVM(args)
		if err != nil {
			return err
		}
		os.Exit(exitCode)
		return nil
	},
}

func init() {
	// Apply env var before registering the flag so it becomes the default.
	if env := os.Getenv("LNX_INSTANCE"); env != "" {
		instanceName = env
	}
	rootCmd.PersistentFlags().StringVar(&instanceName, "instance", instanceName, "VM instance name (default: \"default\")")
	rootCmd.Flags().BoolVarP(&doCheckpoint, "checkpoint", "c", false, "snapshot rootfs before starting the VM")
	rootCmd.Flags().BoolVar(&doEphemeral, "ephemeral", false, "clone rootfs to a temp file; discard on exit")
	rootCmd.Flags().BoolVar(&doSSHAgent, "ssh-agent", false, "forward host SSH agent into the guest")
	rootCmd.Flags().StringArrayVarP(&forwardEnv, "env", "e", nil, "forward a host env var, set KEY=VALUE, or load dotenv vars from @file")
	rootCmd.Flags().BoolVar(&forwardAllEnv, "preserve-env", false, "forward most host environment variables except host-specific path and session vars")

	rootCmd.PersistentPreRunE = func(cmd *cobra.Command, args []string) error {
		// Cobra has parsed flags. Update instanceFlag if --instance was explicitly passed.
		if f := rootCmd.PersistentFlags().Lookup("instance"); f != nil && f.Changed {
			instanceFlag = true
		} else if os.Getenv("LNX_INSTANCE") != "" {
			instanceFlag = true
		}
		return nil
	}
}

func main() {
	initHostLogging()

	// Default to login bash when no command is given.
	if len(os.Args) == 1 {
		os.Args = append(os.Args, "bash", "-l")
	}

	// Strip known lnx flags from args to find the guest command.
	// This lets `lnx --ephemeral bash -l` bypass cobra so `-l`
	// isn't misinterpreted as a flag.
	guestArgs := stripLnxFlags(os.Args[1:])
	if len(guestArgs) > 0 && !isSubcommandOrFlag(guestArgs[0]) {
		if os.Getenv("LNX_INSTANCE") != "" {
			instanceFlag = true
		}
		exitCode, err := runVM(guestArgs)
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		os.Exit(exitCode)
	}

	if err := rootCmd.Execute(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

// stripLnxFlags removes known lnx flags from args before the guest command,
// applying their values to package vars, and returns the remaining args.
// Stops parsing at the first non-flag argument (the guest command).
func stripLnxFlags(args []string) []string {
	i := 0
	for i < len(args) {
		a := args[i]
		switch {
		case a == "--ephemeral":
			doEphemeral = true
			i++
		case a == "--ssh-agent":
			doSSHAgent = true
			i++
		case a == "--preserve-env":
			forwardAllEnv = true
			i++
		case a == "--env" || a == "-e":
			if i+1 >= len(args) {
				return append([]string(nil), args[i:]...)
			}
			forwardEnv = append(forwardEnv, args[i+1])
			i += 2
		case strings.HasPrefix(a, "--env="):
			forwardEnv = append(forwardEnv, strings.TrimPrefix(a, "--env="))
			i++
		case a == "--checkpoint" || a == "-c":
			doCheckpoint = true
			i++
		case a == "--instance" && i+1 < len(args):
			instanceName = args[i+1]
			instanceFlag = true
			i += 2
		case strings.HasPrefix(a, "--instance="):
			instanceName = strings.TrimPrefix(a, "--instance=")
			instanceFlag = true
			i++
		case a == "--":
			// Explicit end of lnx flags — everything after is the guest command.
			return append([]string(nil), args[i+1:]...)
		default:
			// First non-flag arg — everything from here is the guest command.
			return append([]string(nil), args[i:]...)
		}
	}
	return nil
}

func isSubcommandOrFlag(arg string) bool {
	if strings.HasPrefix(arg, "-") {
		return true
	}
	for _, cmd := range rootCmd.Commands() {
		if cmd.Name() == arg {
			return true
		}
	}
	return false
}

func runVM(args []string) (int, error) {
	if err := checkLegacyLayout(); err != nil {
		return -1, err
	}

	// Auto-init on first run if kernel or rootfs is missing.
	// checkImagesVolume runs after auto-init (which creates the volume).
	kernelPath := filepath.Join(lnxBase(), "vmlinuz")
	rootfsPath := resolveRootfsPath()
	if _, err := os.Stat(kernelPath); os.IsNotExist(err) {
		fmt.Fprintln(os.Stderr, "first run — downloading kernel and rootfs...")
		if err := autoInit(); err != nil {
			return -1, fmt.Errorf("auto-init failed: %w", err)
		}
		rootfsPath = resolveRootfsPath()
	} else if _, err := os.Stat(rootfsPath); os.IsNotExist(err) {
		fmt.Fprintf(os.Stderr, "instance %q not initialized — downloading rootfs...\n", instanceName)
		if err := autoInit(); err != nil {
			return -1, fmt.Errorf("auto-init failed: %w", err)
		}
		rootfsPath = resolveRootfsPath()
	}

	if err := checkImagesVolume(); err != nil {
		return -1, err
	}

	// Check if a VM is already running for this instance.
	if !vmIsRunning() {
		// Spawn daemon in background.
		if err := spawnDaemon(); err != nil {
			return -1, err
		}
		if err := waitForVM(60 * time.Second); err != nil {
			return -1, err
		}
	}

	// Exec into the running VM.
	interactive := term.IsTerminal(int(os.Stdin.Fd()))
	execOnce := func() (int, error) {
		if interactive {
			return execInteractive(args)
		}
		return execNonInteractive(args)
	}

	exitCode, err := execOnce()
	if shouldRetryExec(err) {
		if restartErr := restartDaemon(); restartErr == nil {
			return execOnce()
		}
	}
	return exitCode, err
}

// vmIsRunning checks if a VM daemon is running for the current instance.
func vmIsRunning() bool {
	for _, sockPath := range statusSockPaths() {
		conn, err := net.DialTimeout("unix", sockPath, 500*time.Millisecond)
		if err == nil {
			conn.Close()
			return true
		}
	}
	return false
}

// statusSockPaths returns the possible locations for status.sock.
// Normal instances use the instance dir; nested instances use a local work dir.
func statusSockPaths() []string {
	qname := qualifiedInstanceName()
	return []string{
		filepath.Join(instanceDir(), "status.sock"),
		filepath.Join("/var/lib/lnx/instances", qname, "status.sock"),
		filepath.Join("/var/run/lnx", qname, "status.sock"),
	}
}

// spawnDaemon starts the VM daemon as a background process.
func spawnDaemon() error {
	// Remove stale error/spawn logs from any previous daemon run.
	// These may be owned by root (daemon runs as root), so try both.
	qname := qualifiedInstanceName()
	workDir := filepath.Join("/var/lib/lnx/instances", qname)
	os.Remove(filepath.Join(instanceDir(), "error.log"))
	os.Remove(filepath.Join(workDir, "error.log"))
	os.Remove(filepath.Join(workDir, "daemon-spawn.log"))
	os.MkdirAll(workDir, 0777)

	self, err := os.Executable()
	if err != nil {
		return fmt.Errorf("find executable: %w", err)
	}

	daemonArgs := []string{"_daemon", "--instance", instanceName}
	if doCheckpoint {
		daemonArgs = append(daemonArgs, "--checkpoint")
	}
	if doEphemeral {
		daemonArgs = append(daemonArgs, "--ephemeral")
	}
	if doSSHAgent {
		daemonArgs = append(daemonArgs, "--ssh-agent")
	}

	cmd := buildDaemonCmd(self, daemonArgs)
	// Capture daemon stderr for debugging if it fails to start.
	daemonLogDir := filepath.Join("/var/lib/lnx/instances", qname)
	os.MkdirAll(daemonLogDir, 0755)
	if f, err := os.Create(filepath.Join(daemonLogDir, "daemon-spawn.log")); err == nil {
		cmd.Stderr = f
		// f is intentionally not closed — the daemon process owns it.
	}
	cmd.Stdout = nil
	cmd.Stdin = nil
	// Detach from the parent process group so the daemon survives.
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true}

	if err := cmd.Start(); err != nil {
		return fmt.Errorf("start daemon %v: %w", cmd.Args, err)
	}

	slog.Debug("daemon spawned", "pid", cmd.Process.Pid, "args", cmd.Args)

	// Release the process so it doesn't become a zombie.
	cmd.Process.Release()
	return nil
}

func shouldRetryExec(err error) bool {
	if err == nil {
		return false
	}
	if isNoVM(err) {
		return true
	}
	if errors.Is(err, errExecTerminatedUnexpectedly) {
		return true
	}
	s := err.Error()
	return strings.Contains(s, "connect to VM:")
}

func restartDaemon() error {
	if vmIsRunning() {
		req, err := http.NewRequest(http.MethodPost, "http://localhost/stop", nil)
		if err == nil {
			resp, stopErr := apiClient().Do(req)
			if stopErr == nil && resp != nil {
				resp.Body.Close()
			}
		}
		deadline := time.Now().Add(5 * time.Second)
		for vmIsRunning() && time.Now().Before(deadline) {
			time.Sleep(100 * time.Millisecond)
		}
	}
	if vmIsRunning() {
		return fmt.Errorf("VM did not stop cleanly for restart")
	}
	if err := spawnDaemon(); err != nil {
		return err
	}
	return waitForVM(60 * time.Second)
}

func initHostLogging() {
	level := slog.LevelInfo
	switch strings.ToLower(os.Getenv("LNX_LOG")) {
	case "debug":
		level = slog.LevelDebug
	case "warn":
		level = slog.LevelWarn
	case "error":
		level = slog.LevelError
	}

	logDir := instanceDir()
	os.MkdirAll(logDir, 0755)
	f, err := os.OpenFile(filepath.Join(logDir, "lnx.log"), os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0644)
	if err != nil {
		return
	}
	slog.SetDefault(slog.New(slog.NewTextHandler(f, &slog.HandlerOptions{Level: level})))
}

// lnxBase returns the base lnx directory (~/.lnx).
func lnxBase() string {
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".lnx")
}

// instanceDir returns the directory for the current instance's runtime state
// (~/.lnx/instances/<name>). Contains sockets, logs, and other ephemeral files.
func instanceDir() string {
	return instanceDirFor(qualifiedInstanceName())
}

// instanceDirFor returns the runtime state directory for a named instance.
func instanceDirFor(name string) string {
	return filepath.Join(lnxBase(), "instances", name)
}

// imagesDir returns the directory for the current instance's disk images
// (~/.lnx/images/<name>). Contains rootfs, checkpoints, swap, and other
// large files. When an APFS volume is configured, ~/.lnx/images is a
// symlink to the volume mount point.
func imagesDir() string {
	return imagesDirFor(qualifiedInstanceName())
}

// imagesDirFor returns the disk images directory for a named instance.
func imagesDirFor(name string) string {
	return filepath.Join(lnxBase(), "images", name)
}

// resolveRootfsPath returns the rootfs path in the images/ directory.
func resolveRootfsPath() string {
	return filepath.Join(imagesDir(), "rootfs.ext4")
}

// resolveRootfsPathFor returns the rootfs path for a named instance.
func resolveRootfsPathFor(name string) string {
	return filepath.Join(imagesDirFor(name), "rootfs.ext4")
}

// checkLegacyLayout returns an error if rootfs files are found in the old
// instances/ directory layout instead of the new images/ layout.
func checkLegacyLayout() error {
	instancesDir := filepath.Join(lnxBase(), "instances")
	entries, err := os.ReadDir(instancesDir)
	if err != nil {
		return nil // no instances dir at all — fine
	}
	for _, e := range entries {
		if !e.IsDir() {
			continue
		}
		legacy := filepath.Join(instancesDir, e.Name(), "rootfs.ext4")
		if _, err := os.Stat(legacy); err == nil {
			return fmt.Errorf("legacy layout detected: rootfs found at %s\n"+
				"lnx now stores disk images under ~/.lnx/images/ (separate from runtime state in ~/.lnx/instances/).\n"+
				"To migrate, move your rootfs and checkpoint files:\n"+
				"  mkdir -p ~/.lnx/images/%s\n"+
				"  mv %s ~/.lnx/images/%s/\n"+
				"  mv %s/checkpoints ~/.lnx/images/%s/ 2>/dev/null\n"+
				"Repeat for each instance, then re-run your command.",
				legacy, e.Name(), legacy, e.Name(),
				filepath.Join(instancesDir, e.Name()), e.Name())
		}
	}
	return nil
}

// qualifiedInstanceName returns the instance name with parent prefix if nested.
func qualifiedInstanceName() string {
	parent := os.Getenv("LNX_PARENT")
	if parent == "" {
		return instanceName
	}
	return parent + "." + instanceName
}
