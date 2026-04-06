package main

import (
	"path/filepath"

	"github.com/semistrict/lnx"
	"github.com/spf13/cobra"
)

var doGUI bool
var daemonHoldID string

var daemonCmd = &cobra.Command{
	Use:    "_daemon",
	Short:  "Run VM as a background daemon (internal use)",
	Hidden: true,
	RunE: func(cmd *cobra.Command, args []string) error {
		parent, ctx, err := daemonizeCurrentProcess()
		if err != nil {
			return err
		}
		if parent {
			return nil
		}
		defer func() { _ = ctx.Release() }()

		lnx.InitBinary = initBinary
		dir := instanceDir()

		return lnx.RunDaemon(&lnx.Config{
			KernelPath:    filepath.Join(lnxBase(), "vmlinuz"),
			RootfsPath:    filepath.Join(dir, "rootfs.ext4"),
			Hostname:      instanceName + ".lnx",
			Checkpoint:    doCheckpoint,
			Ephemeral:     doEphemeral,
			SSHAgent:      doSSHAgent,
			GUI:           doGUI,
			InitialHoldID: daemonHoldID,
			Shares:        loadShares(dir),
			SocketDir:     dir,
		})
	},
}

func init() {
	daemonCmd.Flags().BoolVarP(&doCheckpoint, "checkpoint", "c", false, "snapshot rootfs before starting")
	daemonCmd.Flags().BoolVar(&doEphemeral, "ephemeral", false, "clone rootfs to a temp file; discard on exit")
	daemonCmd.Flags().BoolVar(&doSSHAgent, "ssh-agent", false, "forward host SSH agent into the guest")
	daemonCmd.Flags().BoolVar(&doGUI, "gui", false, "start graphical desktop")
	daemonCmd.Flags().StringVar(&daemonHoldID, "hold-id", "", "internal initial hold id")
	_ = daemonCmd.Flags().MarkHidden("gui")
	_ = daemonCmd.Flags().MarkHidden("hold-id")
	rootCmd.AddCommand(daemonCmd)
}
