package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/semistrict/lnx"
	"github.com/spf13/cobra"
)

var guiWindowHoldID string
var guiForeground bool

var guiCmd = &cobra.Command{
	Use:   "gui",
	Short: "Start the graphical desktop in the background",
	RunE: func(cmd *cobra.Command, args []string) error {
		return runGUIBackground()
	},
}

var guiWindowCmd = &cobra.Command{
	Use:    "_gui_window",
	Short:  "Wait for the GUI, open a browser window, and release the hold when closed",
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

		return runGUIWindowHelper(guiWindowHoldID)
	},
}

func init() {
	guiCmd.Flags().BoolVarP(&doCheckpoint, "checkpoint", "c", false, "snapshot rootfs before starting the VM")
	guiCmd.Flags().BoolVar(&doEphemeral, "ephemeral", false, "clone rootfs to a temp file; discard on exit")
	guiCmd.Flags().BoolVar(&doSSHAgent, "ssh-agent", false, "forward host SSH agent into the guest")
	guiCmd.Flags().BoolVar(&guiForeground, "foreground", false, "Run in the foreground")
	rootCmd.AddCommand(guiCmd)

	guiWindowCmd.Flags().StringVar(&guiWindowHoldID, "hold-id", "", "internal GUI hold id")
	_ = guiWindowCmd.Flags().MarkHidden("hold-id")
	rootCmd.AddCommand(guiWindowCmd)
}

func runGUIBackground() error {
	alreadyRunning := vmIsRunning()
	var holdID string

	if alreadyRunning {
		gui, err := fetchGUIStatus()
		if err != nil {
			return err
		}
		if !gui.Enabled {
			return fmt.Errorf("VM is already running without GUI; run `lnx stop` and then `lnx gui`")
		}
		id, err := acquireHold(newGUIHoldID(), "gui")
		if err != nil {
			return err
		}
		holdID = id
	} else {
		holdID = newGUIHoldID()
		if err := spawnDaemonWithOptions(holdID, true); err != nil {
			return err
		}
		if err := waitForVM(60 * time.Second); err != nil {
			return err
		}
	}

	if guiForeground {
		return runGUIWindowHelper(holdID)
	}

	if err := spawnGUIWindowHelper(holdID); err != nil {
		_ = releaseHold(holdID)
		return err
	}
	return nil
}

func runGUIWindowHelper(holdID string) error {
	closeReq, controlCleanup, err := startGUIControlServer(holdID)
	if err != nil {
		_ = releaseHold(holdID)
		return err
	}
	defer controlCleanup()

	_ = updateHold(holdID, os.Getpid(), guiControlSocketPath(holdID))

	url, err := waitForGUIURL(60 * time.Second)
	if err != nil {
		_ = releaseHold(holdID)
		return err
	}
	select {
	case <-closeReq:
		_ = releaseHold(holdID)
		return nil
	default:
	}

	cmd, cleanup, err := openBrowserWindow(url)
	if err != nil {
		_ = releaseHold(holdID)
		return err
	}
	if cleanup != nil {
		defer cleanup()
	}

	// If Chrome is unavailable we fall back to `open`, which is not easily
	// trackable. Keep the hold in place so the GUI remains available.
	if cmd == nil {
		slog.Warn("gui: opened via fallback browser launcher; GUI hold will remain until `lnx stop`")
		return nil
	}
	done := make(chan struct{})
	go func() {
		_ = cmd.Wait()
		close(done)
	}()

	select {
	case <-closeReq:
		if cmd.Process != nil {
			_ = cmd.Process.Kill()
		}
		<-done
	case <-done:
	}
	_ = releaseHold(holdID)
	return nil
}

func spawnGUIWindowHelper(holdID string) error {
	self, err := os.Executable()
	if err != nil {
		return fmt.Errorf("find executable: %w", err)
	}

	cmd := exec.Command(self, "_gui_window", "--instance", instanceName, "--hold-id", holdID)
	configureBackgroundCommand(cmd)
	if err := cmd.Start(); err != nil {
		return fmt.Errorf("start GUI window helper: %w", err)
	}
	return cmd.Process.Release()
}

func waitForGUIURL(timeout time.Duration) (string, error) {
	gui, err := fetchGUIStatus()
	if err != nil {
		return "", err
	}
	if !gui.Enabled {
		return "", fmt.Errorf("GUI mode is not enabled for this VM")
	}
	hostPort, err := waitForForwardedPort(6080, timeout)
	if err != nil {
		return "", err
	}
	path := gui.Path
	if path == "" {
		path = "/vnc.html?autoconnect=true&resize=remote"
	}
	return fmt.Sprintf("http://localhost:%d%s", hostPort, path), nil
}

func waitForForwardedPort(guestPort int, timeout time.Duration) (int, error) {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		if hostPort := findForwardedPort(guestPort); hostPort != 0 {
			return hostPort, nil
		}
		time.Sleep(100 * time.Millisecond)
	}
	return 0, fmt.Errorf("forwarded port %d not ready", guestPort)
}

func openBrowserWindow(url string) (*exec.Cmd, func(), error) {
	if chrome := findChromeBinary(); chrome != "" {
		profileDir, err := os.MkdirTemp("", "lnx-gui-chrome-*")
		if err != nil {
			return nil, nil, err
		}
		cleanup := func() { _ = os.RemoveAll(profileDir) }

		cmd := exec.Command(
			chrome,
			"--user-data-dir="+profileDir,
			"--no-first-run",
			"--no-default-browser-check",
			"--app="+url,
		)
		configureDetachedCommand(cmd)
		if err := cmd.Start(); err != nil {
			cleanup()
			return nil, nil, err
		}
		return cmd, cleanup, nil
	}

	cmd := exec.Command("open", url)
	configureDetachedCommand(cmd)
	if err := cmd.Start(); err != nil {
		return nil, nil, err
	}
	_ = cmd.Process.Release()
	return nil, nil, nil
}

func configureDetachedCommand(cmd *exec.Cmd) {
	devNull, err := os.OpenFile(os.DevNull, os.O_RDWR, 0)
	if err == nil {
		cmd.Stdin = devNull
		cmd.Stdout = devNull
		cmd.Stderr = devNull
	}
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
}

func configureBackgroundCommand(cmd *exec.Cmd) {
	devNull, err := os.OpenFile(os.DevNull, os.O_RDWR, 0)
	if err == nil {
		cmd.Stdin = devNull
		cmd.Stdout = devNull
		cmd.Stderr = devNull
	}
	cmd.Env = filteredChildEnv(cmd.Env)
}

func filteredChildEnv(env []string) []string {
	if len(env) == 0 {
		env = os.Environ()
	}
	out := make([]string, 0, len(env))
	for _, kv := range env {
		if strings.HasPrefix(kv, "_GO_DAEMON=") {
			continue
		}
		out = append(out, kv)
	}
	return out
}

func newGUIHoldID() string {
	return fmt.Sprintf("sgui-%d", time.Now().UnixNano())
}

func acquireHold(id, kind string) (string, error) {
	body, err := json.Marshal(map[string]string{
		"id":   id,
		"kind": kind,
	})
	if err != nil {
		return "", err
	}

	resp, err := apiClient().Post("http://localhost/holds/acquire", "application/json", bytes.NewReader(body))
	if err != nil {
		if isNoVM(err) {
			return "", noVMError()
		}
		return "", fmt.Errorf("acquire GUI hold: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		var buf bytes.Buffer
		_, _ = buf.ReadFrom(resp.Body)
		return "", fmt.Errorf("acquire GUI hold failed: %s", buf.String())
	}

	var result struct {
		ID string `json:"id"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&result); err != nil {
		return "", err
	}
	if result.ID == "" {
		return "", fmt.Errorf("empty hold id")
	}
	return result.ID, nil
}

func releaseHold(id string) error {
	if id == "" {
		return nil
	}

	body, err := json.Marshal(map[string]string{"id": id})
	if err != nil {
		return err
	}

	resp, err := apiClient().Post("http://localhost/holds/release", "application/json", bytes.NewReader(body))
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("release GUI hold failed: %s", resp.Status)
	}
	return nil
}

func updateHold(id string, clientPID int, controlSocket string) error {
	if id == "" {
		return nil
	}

	body, err := json.Marshal(map[string]any{
		"id":             id,
		"client_pid":     clientPID,
		"control_socket": controlSocket,
	})
	if err != nil {
		return err
	}

	resp, err := apiClient().Post("http://localhost/holds/update", "application/json", bytes.NewReader(body))
	if err != nil {
		if isNoVM(err) {
			return noVMError()
		}
		return fmt.Errorf("update GUI hold: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("update GUI hold failed: %s", resp.Status)
	}
	return nil
}

func guiControlSocketPath(holdID string) string {
	return filepath.Join(instanceDir(), holdID+".sock")
}

func startGUIControlServer(holdID string) (<-chan struct{}, func(), error) {
	path := guiControlSocketPath(holdID)
	_ = os.Remove(path)
	ln, err := net.Listen("unix", path)
	if err != nil {
		return nil, nil, fmt.Errorf("listen GUI control socket: %w", err)
	}

	closeReq := make(chan struct{})
	var once sync.Once
	signalClose := func() { once.Do(func() { close(closeReq) }) }

	go func() {
		for {
			conn, err := ln.Accept()
			if err != nil {
				return
			}
			func() {
				defer conn.Close()
				line, _ := bufio.NewReader(conn).ReadString('\n')
				if strings.TrimSpace(line) == "close" {
					signalClose()
				}
			}()
		}
	}()

	cleanup := func() {
		_ = ln.Close()
		_ = os.Remove(path)
	}
	return closeReq, cleanup, nil
}

func findChromeBinary() string {
	candidates := []string{
		"google-chrome",
		"google-chrome-stable",
		"chromium",
		"chromium-browser",
		"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
		filepath.Join(os.Getenv("HOME"), "Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
	}
	for _, candidate := range candidates {
		if filepath.IsAbs(candidate) {
			if _, err := os.Stat(candidate); err == nil {
				return candidate
			}
			continue
		}
		if path, err := exec.LookPath(candidate); err == nil {
			return path
		}
	}
	return ""
}

// findForwardedPort checks the ports API for a forwarded guest port.
func findForwardedPort(guestPort int) int {
	client := apiClient()
	resp, err := client.Get("http://localhost/ports")
	if err != nil {
		return 0
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return 0
	}
	var ports []struct {
		Guest int `json:"guest"`
		Host  int `json:"host"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&ports); err != nil {
		return 0
	}
	for _, p := range ports {
		if p.Guest == guestPort {
			return p.Host
		}
	}
	return 0
}

func fetchGUIStatus() (*lnx.GUIStatusResponse, error) {
	resp, err := apiClient().Get("http://localhost/gui")
	if err != nil {
		if isNoVM(err) {
			return nil, noVMError()
		}
		return nil, fmt.Errorf("query GUI status: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("query GUI status failed: %s", resp.Status)
	}

	var gui lnx.GUIStatusResponse
	if err := json.NewDecoder(resp.Body).Decode(&gui); err != nil {
		return nil, fmt.Errorf("read GUI status: %w", err)
	}
	return &gui, nil
}
