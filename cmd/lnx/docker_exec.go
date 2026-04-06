package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"os"
	"strings"
	"time"

	"github.com/semistrict/lnx"
	"nhooyr.io/websocket"
)

type dockerExecCreateRequest struct {
	AttachStdin  bool     `json:"AttachStdin"`
	AttachStdout bool     `json:"AttachStdout"`
	AttachStderr bool     `json:"AttachStderr"`
	Tty          bool     `json:"Tty"`
	Env          []string `json:"Env"`
	Cmd          []string `json:"Cmd"`
	WorkingDir   string   `json:"WorkingDir"`
}

type dockerExecStartRequest struct {
	Detach bool `json:"Detach"`
	Tty    bool `json:"Tty"`
}

func handleDockerExecCreate(w http.ResponseWriter, r *http.Request, container *dockerContainerMetadata) {
	var req dockerExecCreateRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeDockerError(w, http.StatusBadRequest, "bad request: "+err.Error())
		return
	}
	if len(req.Cmd) == 0 {
		writeDockerError(w, http.StatusBadRequest, "Cmd is required")
		return
	}

	id, err := newDockerID()
	if err != nil {
		writeDockerError(w, http.StatusInternalServerError, err.Error())
		return
	}
	meta := &dockerExecMetadata{
		ID:          id,
		ContainerID: container.ID,
		Instance:    container.Instance,
		CreatedAt:   time.Now(),
		ProcessConfig: dockerExecProcessConfig{
			Entrypoint:   append([]string(nil), req.Cmd...),
			Env:          append([]string(nil), req.Env...),
			Tty:          req.Tty,
			WorkingDir:   req.WorkingDir,
			AttachStdin:  req.AttachStdin,
			AttachStdout: req.AttachStdout,
			AttachStderr: req.AttachStderr,
		},
	}
	if meta.ProcessConfig.WorkingDir == "" {
		meta.ProcessConfig.WorkingDir = container.WorkDir
	}
	if err := saveDockerExec(meta); err != nil {
		writeDockerError(w, http.StatusInternalServerError, err.Error())
		return
	}
	writeDockerJSON(w, http.StatusCreated, map[string]any{"Id": meta.ID})
}

func handleDockerExecEndpoint(w http.ResponseWriter, r *http.Request, path string) {
	rest := strings.TrimPrefix(path, "/exec/")
	parts := splitPath(rest)
	if len(parts) == 0 || parts[0] == "" {
		writeDockerError(w, http.StatusNotFound, "exec not found")
		return
	}
	meta, err := loadDockerExec(parts[0])
	if err != nil {
		writeDockerError(w, http.StatusNotFound, err.Error())
		return
	}
	action := ""
	if len(parts) > 1 {
		action = strings.Join(parts[1:], "/")
	}
	switch {
	case r.Method == http.MethodPost && action == "start":
		handleDockerExecStart(w, r, meta)
	case r.Method == http.MethodGet && action == "json":
		handleDockerExecInspect(w, meta)
	default:
		writeDockerError(w, http.StatusNotFound, "unsupported exec endpoint")
	}
}

func handleDockerExecInspect(w http.ResponseWriter, meta *dockerExecMetadata) {
	exitCode := 0
	if meta.ExitCode != nil {
		exitCode = *meta.ExitCode
	}
	writeDockerJSON(w, http.StatusOK, map[string]any{
		"ID":          meta.ID,
		"ContainerID": meta.ContainerID,
		"Running":     meta.Running,
		"ExitCode":    exitCode,
		"Pid":         meta.Pid,
	})
}

func handleDockerExecStart(w http.ResponseWriter, r *http.Request, meta *dockerExecMetadata) {
	slog.Debug("docker exec start begin", "id", meta.ID)
	var req dockerExecStartRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeDockerError(w, http.StatusBadRequest, "bad request: "+err.Error())
		return
	}
	slog.Debug("docker exec start decoded", "id", meta.ID, "detach", req.Detach, "tty", req.Tty)
	_, _ = io.Copy(io.Discard, r.Body)
	_ = r.Body.Close()
	if req.Detach {
		go func(id string) { _ = runDockerExec(id, nil) }(meta.ID)
		w.WriteHeader(http.StatusOK)
		return
	}

	hj, ok := w.(http.Hijacker)
	if !ok {
		writeDockerError(w, http.StatusInternalServerError, "exec start hijack not supported")
		return
	}
	slog.Debug("docker exec start hijacking", "id", meta.ID)
	conn, buf, err := hj.Hijack()
	if err != nil {
		writeDockerError(w, http.StatusInternalServerError, err.Error())
		return
	}
	defer conn.Close()

	_, _ = buf.WriteString("HTTP/1.1 101 UPGRADED\r\nConnection: Upgrade\r\nUpgrade: tcp\r\nContent-Type: application/vnd.docker.raw-stream\r\n\r\n")
	slog.Debug("docker exec start flushing", "id", meta.ID)
	_ = buf.Flush()
	slog.Debug("docker exec start flushed", "id", meta.ID)

	if err := runDockerExec(meta.ID, conn); err != nil {
		slog.Warn("docker exec failed", "id", meta.ID, "error", err)
		return
	}
}

func runDockerExec(id string, stream net.Conn) error {
	meta, err := loadDockerExec(id)
	if err != nil {
		return err
	}
	container, err := loadDockerContainer(meta.ContainerID)
	if err != nil {
		return err
	}
	if container.State != dockerContainerRunning {
		return fmt.Errorf("container %s is not running", containerDisplayName(container))
	}
	if !vmIsRunningForInstance(meta.Instance) {
		if err := waitForVMForInstance(meta.Instance, 60*time.Second); err != nil {
			return err
		}
	}
	now := time.Now()
	meta.StartedAt = &now
	meta.Running = true
	meta.ExitCode = nil
	meta.Pid = os.Getpid()
	if err := saveDockerExec(meta); err != nil {
		return err
	}

	exitCode, runErr := execDockerProcess(meta, stream)
	if runErr != nil {
		slog.Warn("docker exec process failed", "id", meta.ID, "instance", meta.Instance, "error", runErr)
	}

	meta.Running = false
	meta.Pid = 0
	meta.ExitCode = &exitCode
	finished := time.Now()
	meta.FinishedAt = &finished
	if err := saveDockerExec(meta); err != nil {
		return err
	}
	return runErr
}

func execDockerProcess(meta *dockerExecMetadata, stream net.Conn) (int, error) {
	req := lnx.ExecRequest{
		Args:      append([]string(nil), meta.ProcessConfig.Entrypoint...),
		Env:       append([]string(nil), meta.ProcessConfig.Env...),
		CWD:       meta.ProcessConfig.WorkingDir,
		ClientPID: meta.Pid,
		PTY:       meta.ProcessConfig.Tty,
	}
	if meta.ProcessConfig.Tty {
		return execDockerProcessPTY(meta.Instance, req, stream)
	}
	return execDockerProcessStream(meta.Instance, req, stream)
}

func execDockerProcessStream(instance string, req lnx.ExecRequest, stream net.Conn) (int, error) {
	body, err := json.Marshal(req)
	if err != nil {
		return -1, err
	}
	resp, err := apiClientFor(instance).Post("http://localhost/exec", "application/json", bytes.NewReader(body))
	if err != nil {
		return -1, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		var buf bytes.Buffer
		_, _ = buf.ReadFrom(resp.Body)
		return -1, fmt.Errorf("exec failed: %s", bytes.TrimSpace(buf.Bytes()))
	}

	dec := json.NewDecoder(resp.Body)
	exitCode := -1
	for {
		var msg map[string]json.RawMessage
		if err := dec.Decode(&msg); err != nil {
			if err == io.EOF {
				if exitCode == -1 {
					slog.Warn("docker exec stream ended before exit code", "instance", instance)
				}
				return exitCode, nil
			}
			return exitCode, err
		}
		if raw, ok := msg["stdout"]; ok && stream != nil {
			var s string
			_ = json.Unmarshal(raw, &s)
			writeDockerLogChunk(stream, false, 1, []byte(s))
		}
		if raw, ok := msg["stderr"]; ok && stream != nil {
			var s string
			_ = json.Unmarshal(raw, &s)
			writeDockerLogChunk(stream, false, 2, []byte(s))
		}
		if raw, ok := msg["exit_code"]; ok {
			_ = json.Unmarshal(raw, &exitCode)
		}
	}
}

func execDockerProcessPTY(instance string, req lnx.ExecRequest, stream net.Conn) (int, error) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	body, err := json.Marshal(req)
	if err != nil {
		return -1, err
	}
	conn, _, err := websocket.Dial(ctx, "ws://docker/exec/ws", &websocket.DialOptions{
		HTTPClient: apiClientFor(instance),
	})
	if err != nil {
		return -1, err
	}
	defer conn.Close(websocket.StatusNormalClosure, "")

	if err := conn.Write(ctx, websocket.MessageText, body); err != nil {
		return -1, err
	}

	if stream != nil {
		go func() {
			defer cancel()
			buf := make([]byte, 32*1024)
			for {
				n, err := stream.Read(buf)
				if n > 0 {
					_ = conn.Write(ctx, websocket.MessageBinary, buf[:n])
				}
				if err != nil {
					return
				}
			}
		}()
	}

	exitCode := -1
	for {
		typ, data, err := conn.Read(ctx)
		if err != nil {
			if ctx.Err() != nil && stream == nil {
				return exitCode, nil
			}
			return exitCode, err
		}
		switch typ {
		case websocket.MessageBinary:
			if stream != nil {
				if _, err := stream.Write(data); err != nil {
					return exitCode, err
				}
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

func splitPath(s string) []string {
	return strings.FieldsFunc(s, func(r rune) bool { return r == '/' })
}
