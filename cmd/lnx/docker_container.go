package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"

	"github.com/semistrict/lnx"
	"github.com/spf13/cobra"
	"golang.org/x/sys/unix"
	"nhooyr.io/websocket"
)

var dockerContainerID string

var dockerContainerCmd = &cobra.Command{
	Use:    "_docker_container",
	Short:  "Run a Docker-backed container helper (internal use)",
	Hidden: true,
	RunE: func(cmd *cobra.Command, args []string) error {
		return runDockerContainerHelper(dockerContainerID)
	},
}

func init() {
	dockerContainerCmd.Flags().StringVar(&dockerContainerID, "id", "", "internal container id")
	_ = dockerContainerCmd.MarkFlagRequired("id")
	rootCmd.AddCommand(dockerContainerCmd)
}

func createDockerContainer(meta *dockerContainerMetadata) error {
	if err := ensureDockerDirs(); err != nil {
		return err
	}
	if err := os.MkdirAll(dockerContainerDir(meta.ID), 0755); err != nil {
		return err
	}
	if err := os.MkdirAll(dockerContainerInstanceDir(meta.ID), 0755); err != nil {
		return err
	}
	if err := cloneDockerRootfs(meta.ImageDigest, meta.ID); err != nil {
		return err
	}
	return saveDockerContainer(meta)
}

func cloneDockerRootfs(imageDigest, containerID string) error {
	src := dockerImageRootfsPath(imageDigest)
	dst := dockerContainerRootfsPath(containerID)
	if _, err := os.Stat(dst); err == nil {
		return nil
	}
	return unix.Clonefile(src, dst, 0)
}

func spawnDockerContainerHelper(id string) error {
	self, err := os.Executable()
	if err != nil {
		return fmt.Errorf("find executable: %w", err)
	}
	cmd := exec.Command(self, "_docker_container", "--id", id)
	configureBackgroundCommand(cmd)
	if err := cmd.Start(); err != nil {
		return fmt.Errorf("start container helper: %w", err)
	}
	return cmd.Process.Release()
}

func runDockerContainerHelper(id string) error {
	return runDockerContainer(id, nil)
}

func runDockerContainer(id string, live io.Writer) error {
	meta, err := loadDockerContainer(id)
	if err != nil {
		return err
	}
	holdID := "scontainer-" + meta.ID

	meta.Pid = os.Getpid()
	now := time.Now()
	meta.StartedAt = &now
	meta.State = dockerContainerRunning
	meta.ExitCode = nil
	if err := saveDockerContainer(meta); err != nil {
		return err
	}

	logFile, err := os.OpenFile(dockerContainerLogPath(id), os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0644)
	if err != nil {
		return err
	}
	defer logFile.Close()

	if err := spawnDaemonForInstance(meta.Instance, holdID, true); err != nil {
		return finishDockerContainer(meta, 127, err, logFile)
	}
	if err := waitForVMForInstance(meta.Instance, 60*time.Second); err != nil {
		return finishDockerContainer(meta, 127, err, logFile)
	}

	exitCode, err := execDockerContainer(meta, holdID, logFile, live)
	return finishDockerContainer(meta, exitCode, err, logFile)
}

func finishDockerContainer(meta *dockerContainerMetadata, exitCode int, runErr error, logFile *os.File) error {
	if runErr != nil {
		writeDockerLogChunk(logFile, meta.Tty, 2, []byte(runErr.Error()+"\n"))
	}
	meta.State = dockerContainerExited
	meta.Pid = 0
	meta.ExitCode = &exitCode
	finished := time.Now()
	meta.FinishedAt = &finished
	_ = saveDockerWaitResult(meta.ID, exitCode)
	if err := saveDockerContainer(meta); err != nil {
		return err
	}
	if meta.AutoRemove {
		_ = removeDockerContainer(meta.ID, true)
	}
	if runErr != nil {
		return runErr
	}
	return nil
}

func recordDockerContainerFailure(id string, exitCode int, runErr error) {
	if runErr == nil {
		return
	}
	meta, err := loadDockerContainer(id)
	if err != nil {
		return
	}
	if meta.State == dockerContainerExited {
		return
	}
	logFile, err := os.OpenFile(dockerContainerLogPath(id), os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0644)
	if err == nil {
		defer logFile.Close()
		writeDockerLogChunk(logFile, meta.Tty, 2, []byte(runErr.Error()+"\n"))
	}
	meta.State = dockerContainerExited
	meta.Pid = 0
	meta.ExitCode = &exitCode
	finished := time.Now()
	meta.FinishedAt = &finished
	_ = saveDockerWaitResult(meta.ID, exitCode)
	_ = saveDockerContainer(meta)
}

func execDockerContainer(meta *dockerContainerMetadata, holdID string, logFile *os.File, live io.Writer) (int, error) {
	req := lnx.ExecRequest{
		Args:      commandForContainer(meta),
		Env:       meta.Env,
		CWD:       meta.WorkDir,
		ClientPID: meta.Pid,
		PTY:       meta.Tty,
	}
	if meta.Tty {
		return execDockerContainerPTY(meta, holdID, logFile, live, req)
	}
	body, err := json.Marshal(req)
	if err != nil {
		_ = releaseHoldForInstance(meta.Instance, holdID)
		return -1, err
	}
	resp, err := apiClientFor(meta.Instance).Post("http://localhost/exec", "application/json", bytes.NewReader(body))
	if err != nil {
		_ = releaseHoldForInstance(meta.Instance, holdID)
		return -1, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		var buf bytes.Buffer
		_, _ = buf.ReadFrom(resp.Body)
		_ = releaseHoldForInstance(meta.Instance, holdID)
		return -1, fmt.Errorf("container exec failed: %s", strings.TrimSpace(buf.String()))
	}
	_ = releaseHoldForInstance(meta.Instance, holdID)

	scanner := bufio.NewScanner(resp.Body)
	scanner.Buffer(make([]byte, 1024*1024), 1024*1024)
	exitCode := -1
	for scanner.Scan() {
		line := scanner.Bytes()
		var msg map[string]json.RawMessage
		if err := json.Unmarshal(line, &msg); err != nil {
			continue
		}
		if raw, ok := msg["stdout"]; ok {
			var s string
			_ = json.Unmarshal(raw, &s)
			writeDockerLogChunk(logFile, meta.Tty, 1, []byte(s))
			if live != nil {
				writeDockerLogChunk(live, meta.Tty, 1, []byte(s))
			}
		}
		if raw, ok := msg["stderr"]; ok {
			var s string
			_ = json.Unmarshal(raw, &s)
			writeDockerLogChunk(logFile, meta.Tty, 2, []byte(s))
			if live != nil {
				writeDockerLogChunk(live, meta.Tty, 2, []byte(s))
			}
		}
		if raw, ok := msg["exit_code"]; ok {
			_ = json.Unmarshal(raw, &exitCode)
		}
	}
	if err := scanner.Err(); err != nil {
		return exitCode, err
	}
	return exitCode, nil
}

func execDockerContainerPTY(meta *dockerContainerMetadata, holdID string, logFile *os.File, live io.Writer, req lnx.ExecRequest) (int, error) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	body, err := json.Marshal(req)
	if err != nil {
		_ = releaseHoldForInstance(meta.Instance, holdID)
		return -1, err
	}
	conn, _, err := websocket.Dial(ctx, "ws://docker/exec/ws", &websocket.DialOptions{
		HTTPClient: apiClientFor(meta.Instance),
	})
	if err != nil {
		_ = releaseHoldForInstance(meta.Instance, holdID)
		return -1, err
	}
	defer conn.Close(websocket.StatusNormalClosure, "")

	if err := conn.Write(ctx, websocket.MessageText, body); err != nil {
		_ = releaseHoldForInstance(meta.Instance, holdID)
		return -1, err
	}
	_ = releaseHoldForInstance(meta.Instance, holdID)

	if rc, ok := live.(io.Reader); ok && meta.OpenStdin {
		go func() {
			buf := make([]byte, 32*1024)
			for {
				n, err := rc.Read(buf)
				if n > 0 {
					_ = conn.Write(ctx, websocket.MessageBinary, buf[:n])
				}
				if err != nil {
					cancel()
					return
				}
			}
		}()
	}

	exitCode := -1
	for {
		typ, data, err := conn.Read(ctx)
		if err != nil {
			if ctx.Err() != nil {
				return exitCode, nil
			}
			return exitCode, err
		}
		switch typ {
		case websocket.MessageBinary:
			_, _ = logFile.Write(data)
			if live != nil {
				_, _ = live.Write(data)
			}
		case websocket.MessageText:
			var msg map[string]int
			if err := json.Unmarshal(data, &msg); err != nil {
				continue
			}
			if code, ok := msg["exit_code"]; ok {
				exitCode = code
				return exitCode, nil
			}
		}
	}
}

func writeDockerLogChunk(w io.Writer, tty bool, stream byte, data []byte) {
	if len(data) == 0 {
		return
	}
	if tty {
		_, _ = w.Write(data)
		return
	}
	var header [8]byte
	header[0] = stream
	binary.BigEndian.PutUint32(header[4:], uint32(len(data)))
	_, _ = w.Write(header[:])
	_, _ = w.Write(data)
}

func spawnDaemonForInstance(name, holdID string, root bool) error {
	if vmIsRunningForInstance(name) {
		return nil
	}
	self, err := os.Executable()
	if err != nil {
		return fmt.Errorf("find executable: %w", err)
	}
	args := []string{"_daemon", "--instance", name}
	if holdID != "" {
		args = append(args, "--hold-id", holdID)
	}
	if root {
		args = append(args, "--root")
	}
	cmd := exec.Command(self, args...)
	configureBackgroundCommand(cmd)
	if err := cmd.Start(); err != nil {
		return fmt.Errorf("start daemon: %w", err)
	}
	return cmd.Process.Release()
}

func waitForVMForInstance(name string, timeout time.Duration) error {
	sockPath := filepath.Join(lnxBase(), "instances", name, "status.sock")
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		conn, err := net.DialTimeout("unix", sockPath, 500*time.Millisecond)
		if err == nil {
			_ = conn.Close()
			return nil
		}
		time.Sleep(200 * time.Millisecond)
	}
	return fmt.Errorf("timed out waiting for container VM to start")
}

func vmIsRunningForInstance(name string) bool {
	sockPath := filepath.Join(lnxBase(), "instances", name, "status.sock")
	conn, err := net.DialTimeout("unix", sockPath, 500*time.Millisecond)
	if err != nil {
		return false
	}
	_ = conn.Close()
	return true
}

func removeDockerContainer(id string, force bool) error {
	meta, err := loadDockerContainer(id)
	if err != nil {
		return err
	}
	if meta.State == dockerContainerRunning && !force {
		return fmt.Errorf("container %s is running", id)
	}
	_ = os.RemoveAll(dockerContainerInstanceDir(id))
	return os.RemoveAll(dockerContainerDir(id))
}

func releaseHoldForInstance(instance, holdID string) error {
	if holdID == "" {
		return nil
	}
	body, err := json.Marshal(map[string]string{"id": holdID})
	if err != nil {
		return err
	}
	resp, err := apiClientFor(instance).Post("http://localhost/holds/release", "application/json", bytes.NewReader(body))
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("release hold failed: %s", resp.Status)
	}
	return nil
}

func mergeContainerEnv(baseEnv, overrideEnv []string) []string {
	if len(baseEnv) == 0 && len(overrideEnv) == 0 {
		return nil
	}
	merged := map[string]string{}
	order := []string{}
	for _, kv := range append(append([]string(nil), baseEnv...), overrideEnv...) {
		key, value, ok := strings.Cut(kv, "=")
		if !ok {
			continue
		}
		if _, exists := merged[key]; !exists {
			order = append(order, key)
		}
		merged[key] = value
	}
	out := make([]string, 0, len(order))
	for _, key := range order {
		out = append(out, key+"="+merged[key])
	}
	return out
}
