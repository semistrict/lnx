package main

import (
	"archive/tar"
	"bufio"
	"bytes"
	"compress/gzip"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/semistrict/lnx"
	"golang.org/x/sys/unix"
)

type dockerBuildOp string

const (
	dockerBuildFrom       dockerBuildOp = "FROM"
	dockerBuildEnv        dockerBuildOp = "ENV"
	dockerBuildWorkdir    dockerBuildOp = "WORKDIR"
	dockerBuildCopy       dockerBuildOp = "COPY"
	dockerBuildRun        dockerBuildOp = "RUN"
	dockerBuildCmd        dockerBuildOp = "CMD"
	dockerBuildEntrypoint dockerBuildOp = "ENTRYPOINT"
)

type dockerBuildInstruction struct {
	Op       dockerBuildOp
	Value    string
	Args     []string
	JSONArgs []string
}

type dockerBuildSession struct {
	holdID    string
	socketDir string
	sockPath  string
	client    *http.Client
	errCh     chan error
}

func parseDockerfile(r io.Reader) ([]dockerBuildInstruction, error) {
	scanner := bufio.NewScanner(r)
	scanner.Buffer(make([]byte, 0, 64*1024), 1024*1024)

	var (
		lines []string
		cur   string
	)
	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		if strings.HasSuffix(line, "\\") {
			cur += strings.TrimSpace(strings.TrimSuffix(line, "\\")) + " "
			continue
		}
		line = strings.TrimSpace(cur + line)
		cur = ""
		lines = append(lines, line)
	}
	if err := scanner.Err(); err != nil {
		return nil, err
	}
	if strings.TrimSpace(cur) != "" {
		lines = append(lines, strings.TrimSpace(cur))
	}

	out := make([]dockerBuildInstruction, 0, len(lines))
	for _, line := range lines {
		op, rest, ok := strings.Cut(line, " ")
		if !ok {
			return nil, fmt.Errorf("invalid Dockerfile instruction %q", line)
		}
		op = strings.ToUpper(strings.TrimSpace(op))
		rest = strings.TrimSpace(rest)
		inst := dockerBuildInstruction{Op: dockerBuildOp(op), Value: rest}
		switch inst.Op {
		case dockerBuildFrom, dockerBuildRun, dockerBuildWorkdir:
			// Value only.
		case dockerBuildEnv:
			inst.Args = splitDockerArgs(rest)
		case dockerBuildCopy:
			inst.Args = splitDockerArgs(rest)
			if len(inst.Args) < 2 {
				return nil, fmt.Errorf("COPY requires at least two arguments")
			}
		case dockerBuildCmd, dockerBuildEntrypoint:
			if strings.HasPrefix(rest, "[") {
				if err := json.Unmarshal([]byte(rest), &inst.JSONArgs); err != nil {
					return nil, fmt.Errorf("%s json parse: %w", inst.Op, err)
				}
			} else {
				inst.Args = splitDockerArgs(rest)
			}
		default:
			return nil, fmt.Errorf("unsupported Dockerfile instruction %q", op)
		}
		out = append(out, inst)
	}
	return out, nil
}

func splitDockerArgs(s string) []string {
	return strings.Fields(s)
}

func resolveDockerBuildPath(root, rel string) (string, error) {
	if rel == "" {
		return "", fmt.Errorf("empty build path")
	}
	cleanRoot := filepath.Clean(root)
	cleanRel := filepath.Clean(rel)
	full := filepath.Join(cleanRoot, cleanRel)
	if full != cleanRoot && !strings.HasPrefix(full, cleanRoot+string(filepath.Separator)) {
		return "", fmt.Errorf("build path escapes context: %s", rel)
	}
	return full, nil
}

func handleDockerBuild(w http.ResponseWriter, r *http.Request) {
	tag := ""
	if tags := r.URL.Query()["t"]; len(tags) > 0 {
		tag = tags[0]
	}
	dockerfileName := r.URL.Query().Get("dockerfile")
	if dockerfileName == "" {
		dockerfileName = "Dockerfile"
	}

	buildContext, err := io.ReadAll(r.Body)
	if err != nil {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusOK)
		_, _ = fmt.Fprintf(w, "{\"errorDetail\":{\"message\":%q},\"error\":%q}\n", err.Error(), err.Error())
		return
	}

	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusOK)
	flusher, _ := w.(http.Flusher)
	if flusher != nil {
		flusher.Flush()
	}

	meta, err := buildDockerImage(buildContext, dockerfileName, tag, w)
	if err != nil {
		_, _ = fmt.Fprintf(w, "{\"errorDetail\":{\"message\":%q},\"error\":%q}\n", err.Error(), err.Error())
		return
	}
	_ = json.NewEncoder(w).Encode(map[string]any{"aux": map[string]string{"ID": meta.Digest}})
}

func buildDockerImage(buildContext []byte, dockerfileName, tag string, progress io.Writer) (*dockerImageMetadata, error) {
	if err := ensureDockerDirs(); err != nil {
		return nil, err
	}

	contextDir, err := os.MkdirTemp("", "lnx-docker-buildctx-*")
	if err != nil {
		return nil, err
	}
	defer os.RemoveAll(contextDir)

	if err := extractDockerBuildContext(buildContext, contextDir); err != nil {
		return nil, err
	}

	dockerfilePath, err := resolveDockerBuildPath(contextDir, dockerfileName)
	if err != nil {
		return nil, err
	}
	f, err := os.Open(dockerfilePath)
	if err != nil {
		return nil, fmt.Errorf("open Dockerfile: %w", err)
	}
	defer f.Close()

	instrs, err := parseDockerfile(f)
	if err != nil {
		return nil, err
	}

	rootfsPath, meta, err := executeDockerBuild(contextDir, instrs, buildContext, tag, progress)
	if err != nil {
		return nil, err
	}
	if err := os.MkdirAll(dockerImageDir(meta.Digest), 0755); err != nil {
		return nil, err
	}
	if err := os.Rename(rootfsPath, dockerImageRootfsPath(meta.Digest)); err != nil {
		return nil, err
	}
	if err := saveDockerImage(meta); err != nil {
		return nil, err
	}
	return meta, nil
}

func extractDockerBuildContext(data []byte, dst string) error {
	var reader io.Reader = bytes.NewReader(data)
	if len(data) >= 2 && data[0] == 0x1f && data[1] == 0x8b {
		gz, err := gzip.NewReader(bytes.NewReader(data))
		if err != nil {
			return err
		}
		defer gz.Close()
		reader = gz
	}

	tr := tar.NewReader(reader)
	for {
		hdr, err := tr.Next()
		if err == io.EOF {
			return nil
		}
		if err != nil {
			return err
		}
		target, err := resolveDockerBuildPath(dst, hdr.Name)
		if err != nil {
			return err
		}
		switch hdr.Typeflag {
		case tar.TypeDir:
			if err := os.MkdirAll(target, os.FileMode(hdr.Mode)); err != nil {
				return err
			}
		case tar.TypeReg, tar.TypeRegA:
			if err := os.MkdirAll(filepath.Dir(target), 0755); err != nil {
				return err
			}
			f, err := os.OpenFile(target, os.O_CREATE|os.O_TRUNC|os.O_WRONLY, os.FileMode(hdr.Mode))
			if err != nil {
				return err
			}
			if _, err := io.Copy(f, tr); err != nil {
				_ = f.Close()
				return err
			}
			if err := f.Close(); err != nil {
				return err
			}
		}
	}
}

func executeDockerBuild(contextDir string, instrs []dockerBuildInstruction, buildContext []byte, tag string, progress io.Writer) (string, *dockerImageMetadata, error) {
	var (
		rootfsPath string
		config     dockerImageConfig
		parent     *dockerImageMetadata
		session    *dockerBuildSession
	)
	defer func() {
		if session != nil {
			_ = session.Close()
		}
	}()

	for i, inst := range instrs {
		_, _ = fmt.Fprintf(progress, "{\"stream\":%q}\n", fmt.Sprintf("Step %d/%d : %s %s\n", i+1, len(instrs), inst.Op, inst.Value))
		switch inst.Op {
		case dockerBuildFrom:
			if session != nil {
				if err := session.Close(); err != nil {
					return "", nil, err
				}
				session = nil
			}
			parent, rootfsPath, config = nil, "", dockerImageConfig{}
			base, err := ensureDockerImage(inst.Value)
			if err != nil {
				return "", nil, err
			}
			parent = base
			config = base.Config
			rootfsPath, err = cloneDockerBuildRootfs(base.Digest)
			if err != nil {
				return "", nil, err
			}
			session, err = startDockerBuildSession(rootfsPath, contextDir)
			if err != nil {
				return "", nil, err
			}
		case dockerBuildEnv:
			config.Env = mergeContainerEnv(config.Env, inst.Args)
		case dockerBuildWorkdir:
			if rootfsPath == "" {
				return "", nil, fmt.Errorf("WORKDIR before FROM")
			}
			config.WorkingDir = resolveContainerPath(config.WorkingDir, inst.Value)
			if err := session.Run(config.Env, "", "mkdir -p "+shellQuote(config.WorkingDir)); err != nil {
				return "", nil, err
			}
		case dockerBuildCopy:
			if rootfsPath == "" {
				return "", nil, fmt.Errorf("COPY before FROM")
			}
			if err := applyDockerCopyInstruction(session, contextDir, config, inst.Args); err != nil {
				return "", nil, err
			}
		case dockerBuildRun:
			if rootfsPath == "" {
				return "", nil, fmt.Errorf("RUN before FROM")
			}
			if err := session.Run(config.Env, config.WorkingDir, inst.Value); err != nil {
				return "", nil, err
			}
		case dockerBuildCmd:
			config.Cmd = normalizeDockerCommand(inst)
		case dockerBuildEntrypoint:
			config.Entrypoint = normalizeDockerCommand(inst)
		default:
			return "", nil, fmt.Errorf("unsupported build instruction %s", inst.Op)
		}
	}
	if rootfsPath == "" || parent == nil {
		return "", nil, fmt.Errorf("Dockerfile is missing FROM")
	}

	sum := sha256.Sum256(buildContext)
	digest := "sha256:" + hex.EncodeToString(sum[:])
	ref := tag
	if ref == "" {
		ref = digest
	}
	meta := &dockerImageMetadata{
		Reference:    ref,
		CanonicalRef: digest,
		Digest:       digest,
		Layers:       append(append([]string(nil), parent.Layers...), "build:"+hex.EncodeToString(sum[:8])),
		Config:       config,
		CreatedAt:    time.Now(),
		LastUsedAt:   time.Now(),
		ParentDigest: parent.Digest,
	}
	return rootfsPath, meta, nil
}

func cloneDockerBuildRootfs(baseDigest string) (string, error) {
	tmpDir, err := os.MkdirTemp(dockerBaseDir(), "build-rootfs-*")
	if err != nil {
		return "", err
	}
	dst := filepath.Join(tmpDir, "rootfs.ext4")
	if err := unix.Clonefile(dockerImageRootfsPath(baseDigest), dst, 0); err != nil {
		_ = os.RemoveAll(tmpDir)
		return "", err
	}
	return dst, nil
}

func applyDockerCopyInstruction(session *dockerBuildSession, contextDir string, config dockerImageConfig, args []string) error {
	dest := resolveContainerPath(config.WorkingDir, args[len(args)-1])
	srcs := args[:len(args)-1]
	multi := len(srcs) > 1
	for _, src := range srcs {
		hostSrc, err := resolveDockerBuildPath(contextDir, src)
		if err != nil {
			return err
		}
		target := dest
		if multi || strings.HasSuffix(args[len(args)-1], "/") {
			target = resolveContainerPath(dest, filepath.Base(src))
		}
		cmd := "mkdir -p " + shellQuote(filepath.Dir(target)) + " && cp -a " + shellQuote(hostSrc) + " " + shellQuote(target)
		if err := session.Run(config.Env, config.WorkingDir, cmd); err != nil {
			return err
		}
	}
	return nil
}

func startDockerBuildSession(rootfsPath, contextDir string) (*dockerBuildSession, error) {
	lnx.InitBinary = initBinary
	kernelPath := filepath.Join(lnxBase(), "vmlinuz")
	socketDir, err := os.MkdirTemp(dockerBaseDir(), "build-sock-*")
	if err != nil {
		return nil, err
	}
	holdID, err := newDockerID()
	if err != nil {
		_ = os.RemoveAll(socketDir)
		return nil, err
	}

	errCh := make(chan error, 1)
	go func() {
		errCh <- lnx.RunDaemon(&lnx.Config{
			KernelPath:    kernelPath,
			RootfsPath:    rootfsPath,
			CWD:           contextDir,
			Root:          true,
			Hostname:      "docker-build.lnx",
			InitialHoldID: holdID,
			SocketDir:     socketDir,
		})
	}()

	sockPath := filepath.Join(socketDir, "status.sock")
	if err := waitForDockerBuildSocket(sockPath, 60*time.Second); err != nil {
		_ = os.RemoveAll(socketDir)
		return nil, err
	}
	return &dockerBuildSession{
		holdID:    holdID,
		socketDir: socketDir,
		sockPath:  sockPath,
		client:    dockerBuildAPIClient(sockPath),
		errCh:     errCh,
	}, nil
}

func (s *dockerBuildSession) Run(env []string, workdir, shellCmd string) error {
	if workdir != "" {
		shellCmd = "mkdir -p " + shellQuote(workdir) + " && cd " + shellQuote(workdir) + " && " + shellCmd
	}
	exitCode, err := execDockerBuildStep(s.client, shellCmd, env)
	if err != nil {
		return err
	}
	if exitCode != 0 {
		return fmt.Errorf("build step failed with status %d", exitCode)
	}
	return nil
}

func (s *dockerBuildSession) Close() error {
	_, _ = execDockerBuildStep(s.client, "sync", nil)
	body, err := json.Marshal(map[string]string{"id": s.holdID})
	if err == nil {
		resp, reqErr := s.client.Post("http://localhost/holds/release", "application/json", bytes.NewReader(body))
		if reqErr == nil {
			_ = resp.Body.Close()
		}
	}
	waitErr := <-s.errCh
	_ = os.RemoveAll(s.socketDir)
	return waitErr
}

func waitForDockerBuildSocket(sockPath string, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		conn, err := net.DialTimeout("unix", sockPath, 500*time.Millisecond)
		if err == nil {
			_ = conn.Close()
			return nil
		}
		time.Sleep(200 * time.Millisecond)
	}
	return fmt.Errorf("timed out waiting for build VM to start")
}

func execDockerBuildStep(client *http.Client, shellCmd string, env []string) (int, error) {
	body, err := json.Marshal(lnx.ExecRequest{
		Args: []string{"sh", "-lc", shellCmd},
		Env:  env,
	})
	if err != nil {
		return -1, err
	}
	resp, err := client.Post("http://localhost/exec", "application/json", bytes.NewReader(body))
	if err != nil {
		return -1, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		var buf bytes.Buffer
		_, _ = buf.ReadFrom(resp.Body)
		return -1, fmt.Errorf("build exec failed: %s", strings.TrimSpace(buf.String()))
	}
	dec := json.NewDecoder(resp.Body)
	exitCode := -1
	for {
		var msg map[string]json.RawMessage
		if err := dec.Decode(&msg); err != nil {
			if err == io.EOF {
				return exitCode, nil
			}
			return exitCode, err
		}
		if raw, ok := msg["exit_code"]; ok {
			if err := json.Unmarshal(raw, &exitCode); err != nil {
				return exitCode, err
			}
		}
	}
}

func dockerBuildAPIClient(sockPath string) *http.Client {
	return &http.Client{
		Transport: &http.Transport{
			DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
				return (&net.Dialer{}).DialContext(ctx, "unix", sockPath)
			},
		},
	}
}

func normalizeDockerCommand(inst dockerBuildInstruction) []string {
	if len(inst.JSONArgs) > 0 {
		return append([]string(nil), inst.JSONArgs...)
	}
	return append([]string(nil), inst.Args...)
}

func resolveContainerPath(workdir, path string) string {
	if filepath.IsAbs(path) {
		return filepath.Clean(path)
	}
	if workdir == "" {
		workdir = "/"
	}
	return filepath.Clean(filepath.Join(workdir, path))
}

func shellQuote(s string) string {
	return "'" + strings.ReplaceAll(s, "'", `'\''`) + "'"
}
