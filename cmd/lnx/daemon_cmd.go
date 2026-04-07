package main

import (
	"os"
	"path/filepath"

	"github.com/semistrict/lnx"
	"github.com/spf13/cobra"
)

var daemonCmd = &cobra.Command{
	Use:    "_daemon",
	Short:  "Run VM as a background daemon (internal use)",
	Hidden: true,
	RunE: func(cmd *cobra.Command, args []string) error {
		lnx.InitBinary = initBinary
		dir := instanceDir()

		err := lnx.RunDaemon(&lnx.Config{
			KernelPath: filepath.Join(lnxBase(), "vmlinuz"),
			RootfsPath: filepath.Join(dir, "rootfs.ext4"),
			Hostname:   instanceName + ".lnx",
			Checkpoint: doCheckpoint,
			Ephemeral:  doEphemeral,
			SSHAgent:   doSSHAgent,
			Shares:     loadShares(dir),
			SocketDir:  dir,
		})
		if err != nil {
			// Write the error to a file so it's visible even when
			// stderr is nil (daemon spawned by the client).
			errPath := filepath.Join(dir, "error.log")
			os.WriteFile(errPath, []byte(err.Error()+"\n"), 0644)
			return err
		}
		os.Remove(filepath.Join(dir, "error.log"))
		return nil
	},
}

func init() {
	daemonCmd.Flags().BoolVarP(&doCheckpoint, "checkpoint", "c", false, "snapshot rootfs before starting")
	daemonCmd.Flags().BoolVar(&doEphemeral, "ephemeral", false, "clone rootfs to a temp file; discard on exit")
	daemonCmd.Flags().BoolVar(&doSSHAgent, "ssh-agent", false, "forward host SSH agent into the guest")
	rootCmd.AddCommand(daemonCmd)
}
