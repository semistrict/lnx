package main

import (
	"encoding/json"
	"fmt"
	"io"
	"os"
	"strings"
	"syscall"

	"github.com/spf13/cobra"
)

var forkCmd = &cobra.Command{
	Use:   "fork [-- command...]",
	Short: "Fork the running VM into a child instance",
	Long: `Fork creates a copy of the running VM by:
1. Using CRIU to dump all user processes (they keep running in the parent)
2. APFS-cloning the rootfs and CRIU volume (instant, copy-on-write)
3. Booting a child instance that restores the dumped processes

If a command is given after --, it is exec'd in the child with
stdin/stdout/stderr connected to the current terminal — like fork().

Without a command, prints the child instance name.`,
	Args:               cobra.ArbitraryArgs,
	DisableFlagParsing: true,
	RunE:               runFork,
}

func init() {
	rootCmd.AddCommand(forkCmd)
}

func runFork(cmd *cobra.Command, args []string) error {
	// Split args on "--" into fork flags and child command.
	var childArgs []string
	for i, a := range args {
		if a == "--" {
			childArgs = args[i+1:]
			args = args[:i]
			break
		}
	}

	// Handle --help.
	for _, a := range args {
		if a == "-h" || a == "--help" {
			return cmd.Help()
		}
	}

	instanceName := qualifiedInstanceName()
	if !isInstanceRunning(instanceName) {
		return fmt.Errorf("VM must be running to fork")
	}

	client := apiClientFor(instanceName)
	resp, err := client.Post("http://localhost/fork", "application/json", nil)
	if err != nil {
		if isNoVM(err) {
			return noVMError()
		}
		return err
	}
	defer resp.Body.Close()

	if resp.StatusCode/100 != 2 {
		data, _ := io.ReadAll(resp.Body)
		return fmt.Errorf("%s", strings.TrimSpace(string(data)))
	}

	var result struct {
		ChildInstance string `json:"child_instance"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&result); err != nil {
		return fmt.Errorf("decode fork response: %w", err)
	}

	if len(childArgs) == 0 {
		fmt.Printf("forked to %s\n", result.ChildInstance)
		return nil
	}

	// Exec into the child VM with the terminal connected.
	self, err := os.Executable()
	if err != nil {
		return fmt.Errorf("find executable: %w", err)
	}

	execArgs := append([]string{"lnx", "--instance", result.ChildInstance}, childArgs...)
	return syscall.Exec(self, execArgs, os.Environ())
}
