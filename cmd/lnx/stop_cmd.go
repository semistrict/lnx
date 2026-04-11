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

var stopCmd = &cobra.Command{
	Use:   "stop",
	Short: "Stop the running VM",
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

		if killReqCh != nil {
			fmt.Fprintln(os.Stderr, "VM stopping. Press k to kill.")
		} else {
			fmt.Fprintln(os.Stderr, "VM stopping.")
		}

		forced, err := waitForVMStop(killReqCh)
		if err != nil {
			return err
		}

		if forced {
			fmt.Println("VM killed")
			return nil
		}
		fmt.Println("VM stopped")
		return nil
	},
}

func init() {
	rootCmd.AddCommand(stopCmd)
}

func requestVMStop() error {
	resp, err := apiClient().Post("http://localhost/stop", "", nil)
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
