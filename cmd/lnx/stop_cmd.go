package main

import (
	"fmt"
	"net/http"
	"os"
	"path/filepath"
	"syscall"
	"time"

	"github.com/spf13/cobra"
	"golang.org/x/term"
)

var doShutdown bool

var stopCmd = &cobra.Command{
	Use:   "stop",
	Short: "Stop the running VM",
	Long:  "Stop the running VM. By default, the VM is hibernated so the next boot restores instantly. Use --shutdown for a full shutdown.",
	RunE: func(cmd *cobra.Command, args []string) error {
		if err := requestVMStop(); err != nil {
			return err
		}

		killReqCh, restoreTTY, err := watchStopKillKey()
		if err != nil {
			return err
		}
		if restoreTTY != nil {
			defer restoreTTY()
		}

		action := "hibernating"
		if doShutdown {
			action = "stopping"
		}

		if killReqCh != nil {
			fmt.Fprintf(os.Stderr, "VM %s. Press k to kill.\n", action)
		} else {
			fmt.Fprintf(os.Stderr, "VM %s.\n", action)
		}

		forced, err := waitForVMStop(killReqCh)
		if err != nil {
			return err
		}

		if forced {
			fmt.Println("VM killed")
			return nil
		}
		if doShutdown {
			fmt.Println("VM stopped")
		} else {
			fmt.Println("VM hibernated")
		}
		return nil
	},
}

func init() {
	stopCmd.Flags().BoolVar(&doShutdown, "shutdown", false, "full shutdown (discard VM state, no hibernate)")
	rootCmd.AddCommand(stopCmd)
}

func requestVMStop() error {
	url := "http://localhost/stop"
	if doShutdown {
		url += "?mode=shutdown"
	}
	resp, err := apiClient().Post(url, "", nil)
	if err != nil {
		if isNoVM(err) {
			fmt.Fprintln(os.Stderr, "no VM running")
			os.Exit(1)
		}
		return fmt.Errorf("connect to VM: %w", err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("stop failed: %s", resp.Status)
	}
	return nil
}

func watchStopKillKey() (<-chan struct{}, func(), error) {
	if !term.IsTerminal(int(os.Stdin.Fd())) {
		return nil, nil, nil
	}

	oldState, err := term.MakeRaw(int(os.Stdin.Fd()))
	if err != nil {
		return nil, nil, fmt.Errorf("set raw terminal: %w", err)
	}

	restore := func() {
		_ = term.Restore(int(os.Stdin.Fd()), oldState)
	}

	ch := make(chan struct{}, 1)
	go func() {
		var buf [1]byte
		for {
			n, err := os.Stdin.Read(buf[:])
			if err != nil || n == 0 {
				return
			}
			if buf[0] == 'k' || buf[0] == 'K' {
				select {
				case ch <- struct{}{}:
				default:
				}
				return
			}
		}
	}()

	return ch, restore, nil
}

func waitForVMStop(killReqCh <-chan struct{}) (bool, error) {
	ticker := time.NewTicker(100 * time.Millisecond)
	defer ticker.Stop()

	forced := false
	for {
		if !vmIsRunning() {
			return forced, nil
		}

		select {
		case <-ticker.C:
		case <-killReqCh:
			forced = true
			if err := forceKillDaemon(); err != nil {
				return true, err
			}
		}
	}
}

func forceKillDaemon() error {
	pid := readDaemonPID()
	if pid <= 0 {
		return fmt.Errorf("cannot find daemon pid for instance %q", instanceName)
	}

	if err := syscall.Kill(pid, syscall.SIGQUIT); err != nil && err != syscall.ESRCH {
		return fmt.Errorf("send SIGQUIT to daemon %d: %w", pid, err)
	}

	deadline := time.Now().Add(2 * time.Second)
	for processExists(pid) && time.Now().Before(deadline) {
		time.Sleep(100 * time.Millisecond)
	}
	if !processExists(pid) {
		return nil
	}

	_ = syscall.Kill(-pid, syscall.SIGKILL)
	_ = syscall.Kill(pid, syscall.SIGKILL)

	deadline = time.Now().Add(3 * time.Second)
	for processExists(pid) && time.Now().Before(deadline) {
		time.Sleep(100 * time.Millisecond)
	}
	if processExists(pid) {
		return fmt.Errorf("daemon %d did not exit after SIGKILL", pid)
	}
	return nil
}

func readDaemonPID() int {
	return readPIDFile(filepath.Join(instanceDir(), "rootfs.ext4.pid"))
}

func readPIDFile(path string) int {
	data, err := os.ReadFile(path)
	if err != nil {
		return 0
	}
	var pid int
	if _, err := fmt.Sscanf(string(data), "%d", &pid); err != nil {
		return 0
	}
	return pid
}
