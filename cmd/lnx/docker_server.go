package main

import (
	"bufio"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"
)

var (
	dockerAttachMu       sync.Mutex
	dockerPendingAttachs = map[string]*dockerAttachSession{}
)

type dockerAttachSession struct {
	conn net.Conn
	done chan struct{}
}

func newDockerMux() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/", handleDockerAPI)
	return mux
}

func handleDockerAPI(w http.ResponseWriter, r *http.Request) {
	path := trimDockerAPIPath(r.URL.Path)
	slog.Debug("docker api request",
		"method", r.Method,
		"path", path,
		"raw_query", r.URL.RawQuery,
		"upgrade", r.Header.Get("Upgrade"),
		"connection", r.Header.Get("Connection"),
	)
	switch {
	case r.Method == http.MethodHead && path == "/_ping":
		w.WriteHeader(http.StatusOK)
	case r.Method == http.MethodGet && path == "/_ping":
		w.Header().Set("Content-Type", "text/plain")
		_, _ = w.Write([]byte("OK"))
	case r.Method == http.MethodGet && path == "/version":
		writeDockerJSON(w, http.StatusOK, map[string]any{
			"ApiVersion":    "1.54",
			"MinAPIVersion": "1.24",
			"Version":       "0.1",
			"Os":            "linux",
			"Arch":          "arm64",
		})
	case r.Method == http.MethodGet && path == "/info":
		containers, _ := listDockerContainers()
		images, _ := listDockerImages()
		running := 0
		stopped := 0
		for _, c := range containers {
			switch c.State {
			case dockerContainerRunning:
				running++
			default:
				stopped++
			}
		}
		writeDockerJSON(w, http.StatusOK, map[string]any{
			"ID":                "lnx",
			"Containers":        len(containers),
			"ContainersRunning": running,
			"ContainersPaused":  0,
			"ContainersStopped": stopped,
			"Images":            len(images),
			"Driver":            "lnx",
			"DockerRootDir":     dockerBaseDir(),
			"OSType":            "linux",
			"OperatingSystem":   "lnx",
			"Architecture":      "aarch64",
			"ServerVersion":     "0.1",
			"CgroupDriver":      "none",
			"CgroupVersion":     "2",
			"NCPU":              2,
			"MemTotal":          hostMemoryBytesLocal(),
			"Name":              "lnx",
		})
	case r.Method == http.MethodGet && path == "/containers/json":
		containers, err := listDockerContainers()
		if err != nil {
			writeDockerError(w, http.StatusInternalServerError, err.Error())
			return
		}
		all := r.URL.Query().Get("all") == "1" || r.URL.Query().Get("all") == "true"
		var out []map[string]any
		for _, c := range containers {
			if !all && c.State != dockerContainerRunning {
				continue
			}
			status := "Created"
			switch c.State {
			case dockerContainerRunning:
				status = "Up"
			case dockerContainerExited:
				if c.ExitCode != nil {
					status = "Exited (" + itoa(*c.ExitCode) + ")"
				} else {
					status = "Exited"
				}
			}
			entry := map[string]any{
				"Id":      c.ID,
				"Image":   c.Image,
				"Command": strings.Join(commandForContainer(c), " "),
				"Created": c.CreatedAt.Unix(),
				"State":   c.State,
				"Status":  status,
				"Names":   dockerContainerNames(c),
			}
			out = append(out, entry)
		}
		writeDockerJSON(w, http.StatusOK, out)
	case r.Method == http.MethodGet && path == "/images/json":
		images, err := listDockerImages()
		if err != nil {
			writeDockerError(w, http.StatusInternalServerError, err.Error())
			return
		}
		var out []map[string]any
		for _, img := range images {
			out = append(out, map[string]any{
				"Id":       img.Digest,
				"RepoTags": []string{img.Reference},
				"Created":  img.CreatedAt.Unix(),
			})
		}
		writeDockerJSON(w, http.StatusOK, out)
	case r.Method == http.MethodPost && path == "/images/create":
		handleDockerImageCreate(w, r)
	case r.Method == http.MethodPost && path == "/build":
		handleDockerBuild(w, r)
	case r.Method == http.MethodPost && path == "/containers/create":
		handleDockerContainerCreate(w, r)
	case strings.HasPrefix(path, "/exec/"):
		handleDockerExecEndpoint(w, r, path)
	case strings.HasPrefix(path, "/containers/"):
		handleDockerContainerEndpoint(w, r, path)
	default:
		writeDockerError(w, http.StatusNotFound, "unsupported endpoint: "+path)
	}
}

func trimDockerAPIPath(path string) string {
	if strings.HasPrefix(path, "/v") {
		parts := strings.SplitN(path, "/", 3)
		if len(parts) == 3 && looksLikeDockerVersion(parts[1]) {
			return "/" + parts[2]
		}
	}
	return path
}

func looksLikeDockerVersion(s string) bool {
	if len(s) < 2 || s[0] != 'v' {
		return false
	}
	for _, ch := range s[1:] {
		if (ch < '0' || ch > '9') && ch != '.' {
			return false
		}
	}
	return true
}

func writeDockerJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}

func writeDockerError(w http.ResponseWriter, status int, message string) {
	writeDockerJSON(w, status, map[string]string{"message": message})
}

func listDockerContainers() ([]*dockerContainerMetadata, error) {
	entries, err := os.ReadDir(dockerContainersDir())
	if err != nil {
		if os.IsNotExist(err) {
			return nil, nil
		}
		return nil, err
	}
	var out []*dockerContainerMetadata
	for _, entry := range entries {
		if !entry.IsDir() {
			continue
		}
		meta, err := loadDockerContainer(entry.Name())
		if err != nil {
			continue
		}
		out = append(out, meta)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].CreatedAt.Before(out[j].CreatedAt) })
	return out, nil
}

func listDockerImages() ([]*dockerImageMetadata, error) {
	entries, err := os.ReadDir(dockerImagesDir())
	if err != nil {
		if os.IsNotExist(err) {
			return nil, nil
		}
		return nil, err
	}
	var out []*dockerImageMetadata
	for _, entry := range entries {
		if !entry.IsDir() {
			continue
		}
		meta, err := loadDockerImage(filepath.Base(entry.Name()))
		if err != nil {
			continue
		}
		out = append(out, meta)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].CreatedAt.Before(out[j].CreatedAt) })
	return out, nil
}

func dockerContainerNames(meta *dockerContainerMetadata) []string {
	if meta.Name == "" {
		return []string{"/" + meta.ID[:12]}
	}
	return []string{"/" + meta.Name}
}

func commandForContainer(meta *dockerContainerMetadata) []string {
	cmd := append([]string(nil), meta.Entrypoint...)
	cmd = append(cmd, meta.Cmd...)
	return cmd
}

func itoa(v int) string {
	return strconv.Itoa(v)
}

func hostMemoryBytesLocal() uint64 {
	val, err := syscall.Sysctl("hw.memsize")
	if err != nil || len(val) < 8 {
		return 4 << 30
	}
	return binary.LittleEndian.Uint64([]byte(val[:8]))
}

type dockerCreateRequest struct {
	Image      string            `json:"Image"`
	Env        []string          `json:"Env"`
	Cmd        []string          `json:"Cmd"`
	WorkingDir string            `json:"WorkingDir"`
	Entrypoint []string          `json:"Entrypoint"`
	Tty        bool              `json:"Tty"`
	OpenStdin  bool              `json:"OpenStdin"`
	Labels     map[string]string `json:"Labels"`
	HostConfig struct {
		AutoRemove bool `json:"AutoRemove"`
	} `json:"HostConfig"`
}

func handleDockerImageCreate(w http.ResponseWriter, r *http.Request) {
	ref := r.URL.Query().Get("fromImage")
	tag := r.URL.Query().Get("tag")
	if ref == "" {
		writeDockerError(w, http.StatusBadRequest, "fromImage is required")
		return
	}
	if tag != "" && !strings.ContainsAny(ref, "@:") {
		ref += ":" + tag
	}

	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusOK)
	flusher, _ := w.(http.Flusher)
	if flusher != nil {
		flusher.Flush()
	}
	if err := dockerPullProgress(r.Context(), ref, w); err != nil {
		_, _ = fmt.Fprintf(w, "{\"error\":%q}\n", err.Error())
		return
	}
	_, _ = io.WriteString(w, "{\"status\":\"Done\"}\n")
}

func handleDockerContainerCreate(w http.ResponseWriter, r *http.Request) {
	var req dockerCreateRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeDockerError(w, http.StatusBadRequest, "bad request: "+err.Error())
		return
	}
	if req.Image == "" {
		writeDockerError(w, http.StatusBadRequest, "Image is required")
		return
	}

	imageMeta, err := ensureDockerImage(req.Image)
	if err != nil {
		writeDockerError(w, http.StatusNotFound, err.Error())
		return
	}

	name := r.URL.Query().Get("name")
	if name != "" {
		if existing, _ := resolveDockerContainer(name); existing != nil {
			writeDockerError(w, http.StatusConflict, "container name already in use")
			return
		}
	}
	id, err := newDockerID()
	if err != nil {
		writeDockerError(w, http.StatusInternalServerError, err.Error())
		return
	}

	finalEntrypoint := append([]string(nil), imageMeta.Config.Entrypoint...)
	if req.Entrypoint != nil {
		finalEntrypoint = append([]string(nil), req.Entrypoint...)
	}
	finalCmd := append([]string(nil), imageMeta.Config.Cmd...)
	if req.Cmd != nil {
		finalCmd = append([]string(nil), req.Cmd...)
	}
	workDir := imageMeta.Config.WorkingDir
	if req.WorkingDir != "" {
		workDir = req.WorkingDir
	}
	meta := &dockerContainerMetadata{
		ID:          id,
		Name:        name,
		Image:       imageMeta.Reference,
		ImageDigest: imageMeta.Digest,
		Instance:    dockerContainerInstanceName(id),
		CreatedAt:   time.Now(),
		State:       dockerContainerCreated,
		Env:         mergeContainerEnv(imageMeta.Config.Env, req.Env),
		Entrypoint:  finalEntrypoint,
		Cmd:         finalCmd,
		WorkDir:     workDir,
		Tty:         req.Tty,
		OpenStdin:   req.OpenStdin,
		AutoRemove:  req.HostConfig.AutoRemove,
		Labels:      req.Labels,
	}
	if err := createDockerContainer(meta); err != nil {
		writeDockerError(w, http.StatusInternalServerError, err.Error())
		return
	}
	writeDockerJSON(w, http.StatusCreated, map[string]any{"Id": meta.ID, "Warnings": []string{}})
}

func handleDockerContainerEndpoint(w http.ResponseWriter, r *http.Request, path string) {
	rest := strings.TrimPrefix(path, "/containers/")
	parts := strings.Split(rest, "/")
	if len(parts) == 0 || parts[0] == "" {
		writeDockerError(w, http.StatusNotFound, "container not found")
		return
	}
	meta, err := resolveDockerContainer(parts[0])
	if err != nil {
		writeDockerError(w, http.StatusNotFound, err.Error())
		return
	}
	action := ""
	if len(parts) > 1 {
		action = strings.Join(parts[1:], "/")
	}

	switch {
	case r.Method == http.MethodGet && action == "json":
		handleDockerContainerInspect(w, meta)
	case r.Method == http.MethodPost && action == "exec":
		handleDockerExecCreate(w, r, meta)
	case r.Method == http.MethodPost && action == "start":
		handleDockerContainerStart(w, meta)
	case r.Method == http.MethodPost && action == "wait":
		handleDockerContainerWait(w, r, meta)
	case r.Method == http.MethodGet && action == "logs":
		handleDockerContainerLogs(w, r, meta)
	case r.Method == http.MethodPost && action == "stop":
		handleDockerContainerStop(w, meta)
	case r.Method == http.MethodDelete && action == "":
		handleDockerContainerDelete(w, r, meta)
	case r.Method == http.MethodPost && action == "attach":
		handleDockerContainerAttach(w, r, meta)
	default:
		writeDockerError(w, http.StatusNotFound, "unsupported container endpoint")
	}
}

func handleDockerContainerInspect(w http.ResponseWriter, meta *dockerContainerMetadata) {
	status := meta.State
	running := meta.State == dockerContainerRunning
	exitCode := 0
	if meta.ExitCode != nil {
		exitCode = *meta.ExitCode
	}
	writeDockerJSON(w, http.StatusOK, map[string]any{
		"Id":    meta.ID,
		"Name":  "/" + containerDisplayName(meta),
		"Image": meta.ImageDigest,
		"Config": map[string]any{
			"Image":      meta.Image,
			"Entrypoint": meta.Entrypoint,
			"Cmd":        meta.Cmd,
			"Env":        meta.Env,
			"WorkingDir": meta.WorkDir,
			"Tty":        meta.Tty,
			"OpenStdin":  meta.OpenStdin,
			"Labels":     meta.Labels,
		},
		"State": map[string]any{
			"Status":   status,
			"Running":  running,
			"Pid":      meta.Pid,
			"ExitCode": exitCode,
			"StartedAt": func() any {
				if meta.StartedAt == nil {
					return ""
				}
				return meta.StartedAt.Format(time.RFC3339Nano)
			}(),
			"FinishedAt": func() any {
				if meta.FinishedAt == nil {
					return ""
				}
				return meta.FinishedAt.Format(time.RFC3339Nano)
			}(),
		},
	})
}

func handleDockerContainerStart(w http.ResponseWriter, meta *dockerContainerMetadata) {
	if meta.State == dockerContainerRunning {
		w.WriteHeader(http.StatusNoContent)
		return
	}
	if err := os.MkdirAll(dockerContainerDir(meta.ID), 0755); err != nil {
		writeDockerError(w, http.StatusInternalServerError, err.Error())
		return
	}
	if attach := takeDockerAttachSession(meta.ID); attach != nil {
		go func(id string, sess *dockerAttachSession) {
			defer close(sess.done)
			defer sess.conn.Close()
			if err := runDockerContainer(id, sess.conn); err != nil {
				slog.Warn("docker attached container failed", "id", id, "error", err)
				recordDockerContainerFailure(id, 127, err)
			}
		}(meta.ID, attach)
		w.WriteHeader(http.StatusNoContent)
		return
	}
	go func(id string) {
		if err := runDockerContainer(id, nil); err != nil {
			slog.Warn("docker container helper failed", "id", id, "error", err)
			recordDockerContainerFailure(id, 127, err)
		}
	}(meta.ID)
	w.WriteHeader(http.StatusNoContent)
}

func handleDockerContainerWait(w http.ResponseWriter, r *http.Request, meta *dockerContainerMetadata) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusOK)
	if flusher, ok := w.(http.Flusher); ok {
		flusher.Flush()
	}

	condition := r.URL.Query().Get("condition")
	waitRemoved := condition == "removed"
	lastExitCode := 0
	haveExitCode := false

	deadline := time.Now().Add(24 * time.Hour)
	for time.Now().Before(deadline) {
		latest, err := loadDockerContainer(meta.ID)
		if err != nil {
			if waitRemoved && os.IsNotExist(err) {
				if !haveExitCode {
					if code, readErr := loadDockerWaitResult(meta.ID); readErr == nil {
						lastExitCode = code
						haveExitCode = true
					}
				}
				if !haveExitCode {
					lastExitCode = 0
				}
				_ = json.NewEncoder(w).Encode(map[string]any{"StatusCode": lastExitCode})
				return
			}
			break
		}
		if latest.State == dockerContainerExited {
			code := 0
			if latest.ExitCode != nil {
				code = *latest.ExitCode
			}
			lastExitCode = code
			haveExitCode = true
			if !waitRemoved {
				_ = json.NewEncoder(w).Encode(map[string]any{"StatusCode": code})
				return
			}
		}
		select {
		case <-r.Context().Done():
			return
		case <-time.After(200 * time.Millisecond):
		}
	}
	_ = json.NewEncoder(w).Encode(map[string]any{
		"StatusCode": 125,
		"Error":      map[string]any{"Message": "container wait failed"},
	})
}

func handleDockerContainerLogs(w http.ResponseWriter, r *http.Request, meta *dockerContainerMetadata) {
	follow := r.URL.Query().Get("follow") == "1" || r.URL.Query().Get("follow") == "true"
	w.Header().Set("Content-Type", "application/vnd.docker.raw-stream")
	f, err := os.Open(dockerContainerLogPath(meta.ID))
	if err != nil {
		if os.IsNotExist(err) {
			w.WriteHeader(http.StatusOK)
			return
		}
		writeDockerError(w, http.StatusInternalServerError, err.Error())
		return
	}
	defer f.Close()
	flusher, _ := w.(http.Flusher)
	if !follow {
		_, _ = io.Copy(w, f)
		return
	}
	offset, _ := f.Seek(0, io.SeekCurrent)
	buf := make([]byte, 32*1024)
	for {
		n, err := f.Read(buf)
		if n > 0 {
			_, _ = w.Write(buf[:n])
			if flusher != nil {
				flusher.Flush()
			}
			offset += int64(n)
		}
		if err == io.EOF {
			latest, loadErr := loadDockerContainer(meta.ID)
			if loadErr == nil && latest.State == dockerContainerExited {
				slog.Debug("docker attach exiting on stopped container", "id", meta.ID)
				return
			}
			if os.IsNotExist(loadErr) {
				slog.Debug("docker attach exiting on removed container", "id", meta.ID)
				return
			}
			if _, statErr := os.Stat(dockerContainerDir(meta.ID)); os.IsNotExist(statErr) {
				slog.Debug("docker attach exiting on missing container dir", "id", meta.ID)
				return
			}
			select {
			case <-r.Context().Done():
				return
			case <-time.After(200 * time.Millisecond):
			}
			_, _ = f.Seek(offset, io.SeekStart)
			continue
		}
		if err != nil {
			slog.Debug("docker attach exiting on read error", "id", meta.ID, "error", err)
			return
		}
	}
}

func handleDockerContainerStop(w http.ResponseWriter, meta *dockerContainerMetadata) {
	if meta.State != dockerContainerRunning {
		w.WriteHeader(http.StatusNoContent)
		return
	}
	sessions, err := fetchSessions(meta.Instance)
	if err != nil {
		writeDockerError(w, http.StatusInternalServerError, err.Error())
		return
	}
	for i := range sessions {
		if sessions[i].ClientPID != meta.Pid {
			continue
		}
		if err := sendSessionSignal(apiClientFor(meta.Instance), sessions[i].ID, 15, false); err != nil {
			writeDockerError(w, http.StatusInternalServerError, err.Error())
			return
		}
		break
	}
	w.WriteHeader(http.StatusNoContent)
}

func handleDockerContainerDelete(w http.ResponseWriter, r *http.Request, meta *dockerContainerMetadata) {
	force := r.URL.Query().Get("force") == "1" || r.URL.Query().Get("force") == "true"
	if force && meta.State == dockerContainerRunning {
		handleDockerContainerStop(w, meta)
	}
	if err := removeDockerContainer(meta.ID, force); err != nil {
		writeDockerError(w, http.StatusConflict, err.Error())
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

func handleDockerContainerAttach(w http.ResponseWriter, r *http.Request, meta *dockerContainerMetadata) {
	slog.Debug("docker attach", "id", meta.ID, "state", meta.State, "query", r.URL.RawQuery)
	hj, ok := w.(http.Hijacker)
	if !ok {
		writeDockerError(w, http.StatusInternalServerError, "attach not supported")
		return
	}
	conn, buf, err := hj.Hijack()
	if err != nil {
		writeDockerError(w, http.StatusInternalServerError, err.Error())
		return
	}
	defer conn.Close()

	_, _ = buf.WriteString("HTTP/1.1 101 UPGRADED\r\nConnection: Upgrade\r\nUpgrade: tcp\r\nContent-Type: application/vnd.docker.raw-stream\r\n\r\n")
	_ = buf.Flush()

	if meta.State == dockerContainerCreated {
		sess := putDockerAttachSession(meta.ID, conn)
		<-sess.done
		return
	}

	f, err := os.OpenFile(dockerContainerLogPath(meta.ID), os.O_RDONLY|os.O_CREATE, 0644)
	if err != nil {
		return
	}
	defer f.Close()
	var offset int64
	reader := bufio.NewReader(f)
	_ = reader
	data := make([]byte, 32*1024)
	for {
		n, err := f.Read(data)
		if n > 0 {
			_, _ = conn.Write(data[:n])
			offset += int64(n)
		}
		if err == io.EOF {
			latest, loadErr := loadDockerContainer(meta.ID)
			if loadErr == nil && latest.State == dockerContainerExited {
				return
			}
			time.Sleep(100 * time.Millisecond)
			_, _ = f.Seek(offset, io.SeekStart)
			continue
		}
		if err != nil {
			return
		}
	}
}

func resolveDockerContainer(idOrName string) (*dockerContainerMetadata, error) {
	containers, err := listDockerContainers()
	if err != nil {
		return nil, err
	}
	var prefix *dockerContainerMetadata
	for _, c := range containers {
		if c.ID == idOrName || c.Name == idOrName {
			return c, nil
		}
		if strings.HasPrefix(c.ID, idOrName) {
			if prefix != nil {
				return nil, fmt.Errorf("ambiguous container prefix %q", idOrName)
			}
			prefix = c
		}
	}
	if prefix != nil {
		return prefix, nil
	}
	return nil, fmt.Errorf("no such container: %s", idOrName)
}

func containerDisplayName(meta *dockerContainerMetadata) string {
	if meta.Name != "" {
		return meta.Name
	}
	if len(meta.ID) > 12 {
		return meta.ID[:12]
	}
	return meta.ID
}

func putDockerAttachSession(id string, conn net.Conn) *dockerAttachSession {
	dockerAttachMu.Lock()
	defer dockerAttachMu.Unlock()
	sess := &dockerAttachSession{conn: conn, done: make(chan struct{})}
	dockerPendingAttachs[id] = sess
	return sess
}

func takeDockerAttachSession(id string) *dockerAttachSession {
	dockerAttachMu.Lock()
	defer dockerAttachMu.Unlock()
	sess := dockerPendingAttachs[id]
	delete(dockerPendingAttachs, id)
	return sess
}
