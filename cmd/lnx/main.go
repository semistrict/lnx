package main

import (
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"strings"

	"github.com/semistrict/lnx"
	"github.com/spf13/cobra"
)

var doCheckpoint bool

var rootCmd = &cobra.Command{
	Use:           "lnx [flags] [command [args...]]",
	Short:         "Run commands in a lightweight Linux VM",
	SilenceUsage:  true,
	SilenceErrors: true,
	Args:          cobra.ArbitraryArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		exitCode, err := runVM(args)
		if err != nil {
			return err
		}
		os.Exit(exitCode)
		return nil
	},
}

func init() {
	lnx.InitBinary = initBinary
	rootCmd.Flags().BoolVarP(&doCheckpoint, "checkpoint", "c", false, "snapshot rootfs before starting the VM")
}

func main() {
	initHostLogging()

	// Default to bash when no command is given.
	if len(os.Args) == 1 {
		os.Args = append(os.Args, "bash")
	}

	// If the first arg is not a known subcommand or flag, bypass cobra
	// entirely so flags like -g aren't intercepted.
	if len(os.Args) > 1 && !isSubcommandOrFlag(os.Args[1]) {
		exitCode, err := runVM(os.Args[1:])
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
	dir := lnxDir()

	return lnx.Run(&lnx.Config{
		KernelPath: filepath.Join(dir, "vmlinuz"),
		RootfsPath: filepath.Join(dir, "rootfs.ext4"),
		Checkpoint: doCheckpoint,
	}, args...)
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
}

func lnxDir() string {
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".lnx")
}
