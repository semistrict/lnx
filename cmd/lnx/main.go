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

var (
	interactive  bool
	doCheckpoint bool
)

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
	rootCmd.Flags().BoolVarP(&interactive, "interactive", "i", false, "allocate a PTY for interactive use")
	rootCmd.Flags().BoolVarP(&doCheckpoint, "checkpoint", "c", false, "snapshot rootfs before starting the VM")
}

func main() {
	initHostLogging()

	// Default to interactive bash when no command is given.
	// Done here because Cobra shows help instead of calling RunE
	// when subcommands exist and no args are provided.
	if len(os.Args) == 1 {
		os.Args = append(os.Args, "-i", "bash")
	}

	if err := rootCmd.Execute(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func runVM(args []string) (int, error) {
	dir := lnxDir()

	return lnx.Run(&lnx.Config{
		KernelPath:  filepath.Join(dir, "vmlinuz"),
		RootfsPath:  filepath.Join(dir, "rootfs.ext4"),
		Interactive: interactive,
		Checkpoint:  doCheckpoint,
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
	slog.SetDefault(slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: level})))
}

func lnxDir() string {
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".lnx")
}
