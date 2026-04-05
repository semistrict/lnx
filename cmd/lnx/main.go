package main

import (
	"fmt"
	"log/slog"
	"net"
	"os"
	"os/exec"
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
var doGUI bool

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
	rootCmd.Flags().BoolVar(&doGUI, "gui", false, "enable GUI app support (per-app native macOS windows)")

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
		case a == "--gui":
			doGUI = true
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
	dir := instanceDir()

	// Auto-init on first run if kernel or rootfs is missing.
	kernelPath := filepath.Join(lnxBase(), "vmlinuz")
	rootfsPath := filepath.Join(dir, "rootfs.ext4")
	if _, err := os.Stat(kernelPath); os.IsNotExist(err) {
		fmt.Fprintln(os.Stderr, "first run — downloading kernel and rootfs...")
		if err := autoInit(); err != nil {
			return -1, fmt.Errorf("auto-init failed: %w", err)
		}
	} else if _, err := os.Stat(rootfsPath); os.IsNotExist(err) {
		fmt.Fprintf(os.Stderr, "instance %q not initialized — downloading rootfs...\n", instanceName)
		if err := autoInit(); err != nil {
			return -1, fmt.Errorf("auto-init failed: %w", err)
		}
	}

	// Download GUI binaries if --gui is enabled (before daemon spawn so user sees progress).
	if doGUI {
		if err := ensureGUIBinaries(); err != nil {
			return -1, fmt.Errorf("gui setup: %w", err)
		}
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
	if interactive {
		return execInteractive(args)
	}
	return execNonInteractive(args)
}

// vmIsRunning checks if a VM daemon is running for the current instance.
func vmIsRunning() bool {
	sockPath := filepath.Join(instanceDir(), "status.sock")
	conn, err := net.DialTimeout("unix", sockPath, 500*time.Millisecond)
	if err != nil {
		return false
	}
	conn.Close()
	return true
}

// spawnDaemon starts the VM daemon as a background process.
func spawnDaemon() error {
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
	if doGUI {
		daemonArgs = append(daemonArgs, "--gui")
	}

	cmd := exec.Command(self, daemonArgs...)
	cmd.Stdout = nil
	cmd.Stderr = nil
	cmd.Stdin = nil
	// Detach from the parent process group so the daemon survives.
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true}

	if err := cmd.Start(); err != nil {
		return fmt.Errorf("start daemon: %w", err)
	}

	// Release the process so it doesn't become a zombie.
	cmd.Process.Release()
	return nil
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

// instanceDir returns the directory for the current instance (~/.lnx/instances/<name>).
func instanceDir() string {
	return filepath.Join(lnxBase(), "instances", instanceName)
}
