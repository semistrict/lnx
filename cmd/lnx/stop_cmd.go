package main

import (
	"bufio"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"time"

	"github.com/spf13/cobra"
)

var stopCmd = &cobra.Command{
	Use:   "stop",
	Short: "Stop the running VM",
	RunE: func(cmd *cobra.Command, args []string) error {
		if err := requestVMStop(); err != nil {
			return err
		}

		killReqCh, err := watchStopKillKey()
		if err != nil {
			return err
		}
		if killReqCh != nil {
			writeTerminalStatusLine(os.Stderr, "VM stopping. Press k then Enter to kill.", false)
		} else {
			writeTerminalStatusLine(os.Stderr, "VM stopping.", false)
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

func writeTerminalStatusLine(w io.Writer, line string, rawTTY bool) {
	if rawTTY {
		fmt.Fprint(w, line, "\r\n")
		return
	}
	fmt.Fprintln(w, line)
}

func shouldForceKillInput(line string) bool {
	return strings.EqualFold(strings.TrimSpace(line), "k")
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

func watchStopKillKey() (<-chan struct{}, error) {
	ch := make(chan struct{}, 1)
	go func() {
		reader := bufio.NewReader(os.Stdin)
		for {
			line, err := reader.ReadString('\n')
			if err != nil {
				return
			}
			if shouldForceKillInput(line) {
				select {
				case ch <- struct{}{}:
				default:
				}
				return
			}
		}
	}()

	return ch, nil
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
