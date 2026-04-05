package main

import (
	"fmt"
	"os"

	"github.com/spf13/cobra"
)

var stopCmd = &cobra.Command{
	Use:   "stop",
	Short: "Stop the running VM",
	RunE: func(cmd *cobra.Command, args []string) error {
		resp, err := apiClient().Post("http://localhost/stop", "", nil)
		if err != nil {
			if isNoVM(err) {
				fmt.Fprintln(os.Stderr, "no VM running")
				os.Exit(1)
			}
			return fmt.Errorf("connect to VM: %w", err)
		}
		resp.Body.Close()
		fmt.Println("VM stopping")
		return nil
	},
}

func init() {
	rootCmd.AddCommand(stopCmd)
}
