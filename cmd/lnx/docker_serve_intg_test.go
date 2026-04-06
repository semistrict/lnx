//go:build darwin && integration

package main

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestServeDockerRunForegroundAttach(t *testing.T) {
	socket, cleanup := startDockerServeForTest(t)
	defer cleanup()

	out := runDockerCLI(t, socket,
		"run", "--rm", "alpine:latest",
		"sh", "-lc", "echo foreground-ok && echo foreground-err >&2",
	)
	if !strings.Contains(out, "foreground-ok") {
		t.Fatalf("stdout missing from foreground run: %q", out)
	}
	if !strings.Contains(out, "foreground-err") {
		t.Fatalf("stderr missing from foreground run: %q", out)
	}
}

func TestServeDockerExecForegroundAndDetached(t *testing.T) {
	socket, cleanup := startDockerServeForTest(t)
	defer cleanup()

	name := fmt.Sprintf("lnx-exec-%d", time.Now().UnixNano())
	runDockerCLI(t, socket, "run", "-d", "--name", name, "alpine:latest", "tail", "-f", "/dev/null")
	t.Cleanup(func() {
		_ = exec.Command("docker", "--host", "unix://"+socket, "rm", "-f", name).Run()
	})

	out := runDockerCLI(t, socket, "exec", name, "sh", "-lc", "echo exec-foreground")
	if !strings.Contains(out, "exec-foreground") {
		t.Fatalf("foreground exec output missing: %q", out)
	}

	runDockerCLI(t, socket, "exec", "-d", name, "sh", "-lc", "echo exec-detached > /tmp/exec-result")
	out = runDockerCLI(t, socket, "exec", name, "cat", "/tmp/exec-result")
	if !strings.Contains(out, "exec-detached") {
		t.Fatalf("detached exec result missing: %q", out)
	}
}

func TestServeDockerBuild(t *testing.T) {
	socket, cleanup := startDockerServeForTest(t)
	defer cleanup()

	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "msg.txt"), []byte("built-image\n"), 0644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "Dockerfile"), []byte(`
FROM alpine:latest
WORKDIR /app
COPY msg.txt /app/msg.txt
RUN echo built-run > /app/run.txt
CMD ["sh","-lc","cat /app/msg.txt && cat /app/run.txt"]
`), 0644); err != nil {
		t.Fatal(err)
	}

	tag := fmt.Sprintf("lnx-build-%d", time.Now().UnixNano())
	runDockerCLIInDir(t, dir, socket, "build", "-t", tag, ".")
	t.Cleanup(func() {
		_ = exec.Command("docker", "--host", "unix://"+socket, "rmi", "-f", tag).Run()
	})

	out := runDockerCLI(t, socket, "run", "--rm", tag)
	if !strings.Contains(out, "built-image") || !strings.Contains(out, "built-run") {
		t.Fatalf("built image output mismatch: %q", out)
	}
}

func TestServeDockerDetachedRun(t *testing.T) {
	socket, cleanup := startDockerServeDetachedForTest(t)
	defer cleanup()

	runDockerCLI(t, socket, "pull", "alpine:latest")
	out := runDockerCLI(t, socket, "run", "alpine:latest")
	if strings.TrimSpace(out) != "" {
		t.Fatalf("detached run expected no output, got: %q", out)
	}
}

func startDockerServeForTest(t *testing.T) (string, func()) {
	t.Helper()
	lnxBin := requireLnxBinary(t)
	requireTool(t, "docker")

	socket := filepath.Join(os.TempDir(), fmt.Sprintf("lnx-docker-%d.sock", time.Now().UnixNano()))
	_ = os.Remove(socket)
	cmd := exec.Command(lnxBin, "serve-docker", "--foreground", "--socket", socket)
	cmd.Env = append(os.Environ(), "LNX_LOG=debug")
	if err := cmd.Start(); err != nil {
		t.Fatalf("start serve-docker: %v", err)
	}

	waitForSocket(t, socket)
	cleanup := func() {
		if cmd.Process != nil {
			_ = cmd.Process.Kill()
			_, _ = cmd.Process.Wait()
		}
		_ = os.Remove(socket)
	}
	return socket, cleanup
}

func startDockerServeDetachedForTest(t *testing.T) (string, func()) {
	t.Helper()
	lnxBin := requireLnxBinary(t)
	requireTool(t, "docker")
	requireTool(t, "pkill")

	socket := filepath.Join(os.TempDir(), fmt.Sprintf("lnx-docker-detached-%d.sock", time.Now().UnixNano()))
	_ = os.Remove(socket)

	cmd := exec.Command(lnxBin, "serve-docker", "--socket", socket)
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("start detached serve-docker: %v\n%s", err, out)
	}

	waitForSocket(t, socket)
	cleanup := func() {
		_ = exec.Command("pkill", "-f", "_serve_docker .*"+socket).Run()
		_ = os.Remove(socket)
	}
	return socket, cleanup
}

func requireLnxBinary(t *testing.T) string {
	t.Helper()
	root := repoRoot(t)
	bin := filepath.Join(t.TempDir(), "lnx")
	cmd := exec.Command("go", "build", "-o", bin, "./cmd/lnx")
	cmd.Dir = root
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("build lnx test binary: %v\n%s", err, out)
	}
	sign := exec.Command("codesign", "--entitlements", filepath.Join(root, "entitlements.plist"), "--force", "-s", "-", bin)
	sign.Dir = root
	if out, err := sign.CombinedOutput(); err != nil {
		t.Fatalf("codesign lnx test binary: %v\n%s", err, out)
	}
	return bin
}

func repoRoot(t *testing.T) string {
	t.Helper()
	dir, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	for {
		if _, err := os.Stat(filepath.Join(dir, "go.mod")); err == nil {
			return dir
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			t.Fatal("could not find repo root")
		}
		dir = parent
	}
}

func requireTool(t *testing.T, name string) {
	t.Helper()
	if _, err := exec.LookPath(name); err != nil {
		t.Skipf("skipping: %s not found", name)
	}
}

func waitForSocket(t *testing.T, path string) {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		if _, err := os.Stat(path); err == nil {
			return
		}
		time.Sleep(50 * time.Millisecond)
	}
	t.Fatalf("timed out waiting for socket %s", path)
}

func runDockerCLI(t *testing.T, socket string, args ...string) string {
	t.Helper()
	return runDockerCLIInDir(t, "", socket, args...)
}

func runDockerCLIInDir(t *testing.T, dir, socket string, args ...string) string {
	t.Helper()
	cmd := exec.Command("docker", append([]string{"--host", "unix://" + socket}, args...)...)
	cmd.Env = append(os.Environ(), "DOCKER_BUILDKIT=0")
	if dir != "" {
		cmd.Dir = dir
	}
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("docker %v failed: %v\n%s", args, err, out)
	}
	return string(out)
}
