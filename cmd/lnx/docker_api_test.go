package main

import (
	"archive/tar"
	"bufio"
	"bytes"
	"encoding/json"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestTrimDockerAPIPath(t *testing.T) {
	tests := []struct {
		in   string
		want string
	}{
		{in: "/_ping", want: "/_ping"},
		{in: "/v1.54/_ping", want: "/_ping"},
		{in: "/v1.54/containers/json", want: "/containers/json"},
		{in: "/vbogus/containers/json", want: "/vbogus/containers/json"},
	}

	for _, tt := range tests {
		if got := trimDockerAPIPath(tt.in); got != tt.want {
			t.Fatalf("trimDockerAPIPath(%q) = %q, want %q", tt.in, got, tt.want)
		}
	}
}

func TestMergeContainerEnvOverridePreservesOrder(t *testing.T) {
	got := mergeContainerEnv(
		[]string{"A=1", "B=2", "PATH=/bin"},
		[]string{"B=3", "C=4", "PATH=/usr/bin"},
	)
	want := []string{"A=1", "B=3", "PATH=/usr/bin", "C=4"}
	if len(got) != len(want) {
		t.Fatalf("mergeContainerEnv length = %d, want %d (%v)", len(got), len(want), got)
	}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("mergeContainerEnv[%d] = %q, want %q (full=%v)", i, got[i], want[i], got)
		}
	}
}

func TestHandleDockerContainerWaitFlushesBeforeExit(t *testing.T) {
	withDockerTestHome(t)
	meta := createTestContainer(t, &dockerContainerMetadata{
		ID:        "wait-flush",
		Image:     "alpine:latest",
		Instance:  "docker-wait",
		CreatedAt: time.Now(),
		State:     dockerContainerRunning,
	})

	req := httptest.NewRequest("POST", "/containers/"+meta.ID+"/wait?condition=next-exit", nil)
	rec := httptest.NewRecorder()

	done := make(chan struct{})
	go func() {
		handleDockerContainerWait(rec, req, meta)
		close(done)
	}()

	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if rec.Code == 200 && rec.Flushed {
			break
		}
		time.Sleep(10 * time.Millisecond)
	}
	if rec.Code != 200 {
		t.Fatalf("wait status code = %d, want 200", rec.Code)
	}
	if !rec.Flushed {
		t.Fatal("wait handler did not flush headers before container exit")
	}

	exitCode := 7
	now := time.Now()
	meta.State = dockerContainerExited
	meta.ExitCode = &exitCode
	meta.FinishedAt = &now
	if err := saveDockerContainer(meta); err != nil {
		t.Fatal(err)
	}

	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("wait handler did not complete after container exit")
	}

	var body map[string]any
	if err := json.Unmarshal(rec.Body.Bytes(), &body); err != nil {
		t.Fatalf("decode wait response: %v", err)
	}
	if got := int(body["StatusCode"].(float64)); got != 7 {
		t.Fatalf("StatusCode = %d, want 7", got)
	}
}

func TestHandleDockerContainerWaitConditionRemoved(t *testing.T) {
	withDockerTestHome(t)
	meta := createTestContainer(t, &dockerContainerMetadata{
		ID:         "wait-removed",
		Image:      "alpine:latest",
		Instance:   "docker-wait-removed",
		CreatedAt:  time.Now(),
		State:      dockerContainerRunning,
		AutoRemove: true,
	})

	req := httptest.NewRequest("POST", "/containers/"+meta.ID+"/wait?condition=removed", nil)
	rec := httptest.NewRecorder()

	done := make(chan struct{})
	go func() {
		handleDockerContainerWait(rec, req, meta)
		close(done)
	}()

	exitCode := 9
	finished := time.Now()
	meta.State = dockerContainerExited
	meta.ExitCode = &exitCode
	meta.FinishedAt = &finished
	if err := saveDockerContainer(meta); err != nil {
		t.Fatal(err)
	}
	if err := saveDockerWaitResult(meta.ID, exitCode); err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(dockerContainerMetadataPath(meta.ID)); err != nil {
		t.Fatal(err)
	}

	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("wait removed handler did not complete after removal")
	}

	var body map[string]any
	if err := json.Unmarshal(rec.Body.Bytes(), &body); err != nil {
		t.Fatalf("decode wait removed response: %v", err)
	}
	if got := int(body["StatusCode"].(float64)); got != 9 {
		t.Fatalf("StatusCode = %d, want 9", got)
	}
}

func TestHandleDockerContainerAttachStreamsAndStopsOnExit(t *testing.T) {
	withDockerTestHome(t)
	meta := createTestContainer(t, &dockerContainerMetadata{
		ID:        "attach-stream",
		Image:     "alpine:latest",
		Instance:  "docker-attach",
		CreatedAt: time.Now(),
		State:     dockerContainerRunning,
	})

	hj := newHijackRecorder(t)
	req := httptest.NewRequest("POST", "/containers/"+meta.ID+"/attach", nil)

	done := make(chan struct{})
	go func() {
		handleDockerContainerAttach(hj, req, meta)
		close(done)
	}()

	header := readUntil(t, hj.client, "\r\n\r\n")
	if !strings.Contains(header, "101 UPGRADED") {
		t.Fatalf("unexpected hijack response header: %q", header)
	}

	logData := []byte{
		1, 0, 0, 0, 0, 0, 0, 3, 'o', 'k', '\n',
		2, 0, 0, 0, 0, 0, 0, 4, 'e', 'r', 'r', '\n',
	}
	logPath := dockerContainerLogPath(meta.ID)
	if err := os.WriteFile(logPath, logData[:11], 0644); err != nil {
		t.Fatal(err)
	}

	first := readExactly(t, hj.client, 11)
	if !bytes.Equal(first, logData[:11]) {
		t.Fatalf("first attach chunk = %v, want %v", first, logData[:11])
	}

	f, err := os.OpenFile(logPath, os.O_WRONLY|os.O_APPEND, 0644)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := f.Write(logData[11:]); err != nil {
		t.Fatal(err)
	}
	_ = f.Close()

	second := readExactly(t, hj.client, len(logData[11:]))
	if !bytes.Equal(second, logData[11:]) {
		t.Fatalf("second attach chunk = %v, want %v", second, logData[11:])
	}

	exitCode := 0
	finished := time.Now()
	meta.State = dockerContainerExited
	meta.ExitCode = &exitCode
	meta.FinishedAt = &finished
	if err := saveDockerContainer(meta); err != nil {
		t.Fatal(err)
	}

	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("attach handler did not exit after container stopped")
	}
}

func TestParseDockerfileBasicInstructions(t *testing.T) {
	df := strings.NewReader(`
FROM alpine:latest
ENV A=1 B=2
WORKDIR /app
COPY hello.txt /app/hello.txt
RUN echo built > /app/out.txt
ENTRYPOINT ["cat"]
CMD ["/app/hello.txt"]
`)

	instrs, err := parseDockerfile(df)
	if err != nil {
		t.Fatalf("parseDockerfile: %v", err)
	}
	if len(instrs) != 7 {
		t.Fatalf("instruction count = %d, want 7", len(instrs))
	}
	if instrs[0].Op != dockerBuildFrom || instrs[0].Value != "alpine:latest" {
		t.Fatalf("FROM = %+v", instrs[0])
	}
	if instrs[2].Op != dockerBuildWorkdir || instrs[2].Value != "/app" {
		t.Fatalf("WORKDIR = %+v", instrs[2])
	}
	if instrs[3].Op != dockerBuildCopy || len(instrs[3].Args) != 2 || instrs[3].Args[0] != "hello.txt" || instrs[3].Args[1] != "/app/hello.txt" {
		t.Fatalf("COPY = %+v", instrs[3])
	}
	if instrs[5].JSONArgs == nil || len(instrs[5].JSONArgs) != 1 || instrs[5].JSONArgs[0] != "cat" {
		t.Fatalf("ENTRYPOINT = %+v", instrs[5])
	}
}

func TestResolveDockerBuildPathRejectsTraversal(t *testing.T) {
	root := t.TempDir()
	if _, err := resolveDockerBuildPath(root, "../etc/passwd"); err == nil {
		t.Fatal("expected path traversal to fail")
	}
}

func TestDockerExecMetadataRoundTrip(t *testing.T) {
	withDockerTestHome(t)
	meta := &dockerExecMetadata{
		ID:          "exec-123",
		ContainerID: "container-123",
		Instance:    "docker-container-123",
		CreatedAt:   time.Now(),
		Running:     true,
		ProcessConfig: dockerExecProcessConfig{
			Entrypoint: []string{"sh", "-lc", "echo hi"},
			Env:        []string{"A=1"},
			Tty:        true,
			WorkingDir: "/work",
		},
	}
	if err := saveDockerExec(meta); err != nil {
		t.Fatal(err)
	}
	got, err := loadDockerExec(meta.ID)
	if err != nil {
		t.Fatal(err)
	}
	if got.ID != meta.ID || got.ContainerID != meta.ContainerID || got.ProcessConfig.WorkingDir != "/work" || !got.ProcessConfig.Tty {
		t.Fatalf("round-trip exec metadata mismatch: %+v", got)
	}
}

func TestHandleDockerExecCreatePersistsMetadata(t *testing.T) {
	withDockerTestHome(t)
	container := createTestContainer(t, &dockerContainerMetadata{
		ID:        "exec-create-container",
		Image:     "alpine:latest",
		Instance:  "docker-exec-create",
		CreatedAt: time.Now(),
		State:     dockerContainerRunning,
		WorkDir:   "/work",
	})

	body := strings.NewReader(`{"AttachStdout":true,"AttachStderr":true,"Tty":false,"Cmd":["sh","-lc","echo hi"]}`)
	req := httptest.NewRequest("POST", "/containers/"+container.ID+"/exec", body)
	rec := httptest.NewRecorder()

	handleDockerExecCreate(rec, req, container)

	if rec.Code != 201 {
		t.Fatalf("exec create status = %d, want 201", rec.Code)
	}
	var resp map[string]string
	if err := json.Unmarshal(rec.Body.Bytes(), &resp); err != nil {
		t.Fatalf("decode exec create response: %v", err)
	}
	meta, err := loadDockerExec(resp["Id"])
	if err != nil {
		t.Fatalf("load exec metadata: %v", err)
	}
	if meta.ContainerID != container.ID || meta.ProcessConfig.WorkingDir != "/work" {
		t.Fatalf("unexpected exec metadata: %+v", meta)
	}
}

func TestExtractDockerBuildContextWritesFiles(t *testing.T) {
	tmp := t.TempDir()

	var buf bytes.Buffer
	tw := tar.NewWriter(&buf)
	writeTarFile(t, tw, "Dockerfile", []byte("FROM alpine:latest\n"))
	writeTarFile(t, tw, "subdir/hello.txt", []byte("hello\n"))
	if err := tw.Close(); err != nil {
		t.Fatal(err)
	}

	if err := extractDockerBuildContext(buf.Bytes(), tmp); err != nil {
		t.Fatalf("extractDockerBuildContext: %v", err)
	}
	if data, err := os.ReadFile(filepath.Join(tmp, "subdir", "hello.txt")); err != nil || string(data) != "hello\n" {
		t.Fatalf("unexpected extracted file: data=%q err=%v", string(data), err)
	}
}

type hijackRecorder struct {
	header http.Header
	client net.Conn
	server net.Conn
}

func newHijackRecorder(t *testing.T) *hijackRecorder {
	t.Helper()
	client, server := net.Pipe()
	t.Cleanup(func() {
		_ = client.Close()
		_ = server.Close()
	})
	return &hijackRecorder{
		header: make(http.Header),
		client: client,
		server: server,
	}
}

func (h *hijackRecorder) Header() http.Header {
	return h.header
}

func (h *hijackRecorder) Write(p []byte) (int, error) {
	return len(p), nil
}

func (h *hijackRecorder) WriteHeader(statusCode int) {}

func (h *hijackRecorder) Hijack() (net.Conn, *bufio.ReadWriter, error) {
	rw := bufio.NewReadWriter(bufio.NewReader(h.server), bufio.NewWriter(h.server))
	return h.server, rw, nil
}

func withDockerTestHome(t *testing.T) string {
	t.Helper()
	home := t.TempDir()
	t.Setenv("HOME", home)
	return home
}

func createTestContainer(t *testing.T, meta *dockerContainerMetadata) *dockerContainerMetadata {
	t.Helper()
	if err := ensureDockerDirs(); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(dockerContainerDir(meta.ID), 0755); err != nil {
		t.Fatal(err)
	}
	if err := saveDockerContainer(meta); err != nil {
		t.Fatal(err)
	}
	return meta
}

func readUntil(t *testing.T, conn net.Conn, marker string) string {
	t.Helper()
	var buf bytes.Buffer
	tmp := make([]byte, 1)
	deadline := time.Now().Add(2 * time.Second)
	for {
		if time.Now().After(deadline) {
			t.Fatalf("timed out waiting for marker %q in %q", marker, buf.String())
		}
		if err := conn.SetReadDeadline(time.Now().Add(100 * time.Millisecond)); err != nil {
			t.Fatal(err)
		}
		n, err := conn.Read(tmp)
		if n > 0 {
			buf.Write(tmp[:n])
			if strings.Contains(buf.String(), marker) {
				return buf.String()
			}
		}
		if ne, ok := err.(net.Error); ok && ne.Timeout() {
			continue
		}
		if err != nil {
			t.Fatalf("readUntil(%q): %v", marker, err)
		}
	}
}

func readExactly(t *testing.T, conn net.Conn, n int) []byte {
	t.Helper()
	out := make([]byte, n)
	if err := conn.SetReadDeadline(time.Now().Add(2 * time.Second)); err != nil {
		t.Fatal(err)
	}
	if _, err := io.ReadFull(conn, out); err != nil {
		t.Fatalf("readExactly(%d): %v", n, err)
	}
	return out
}

func writeTarFile(t *testing.T, tw *tar.Writer, name string, data []byte) {
	t.Helper()
	if err := tw.WriteHeader(&tar.Header{
		Name: name,
		Mode: 0644,
		Size: int64(len(data)),
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := tw.Write(data); err != nil {
		t.Fatal(err)
	}
}

func TestDockerExecPathHelpers(t *testing.T) {
	withDockerTestHome(t)
	id := "exec-path"
	wantDir := filepath.Join(lnxBase(), "docker", "execs", id)
	if got := dockerExecDir(id); got != wantDir {
		t.Fatalf("dockerExecDir = %q, want %q", got, wantDir)
	}
	if got := dockerExecMetadataPath(id); got != filepath.Join(wantDir, "exec.json") {
		t.Fatalf("dockerExecMetadataPath = %q", got)
	}
}
