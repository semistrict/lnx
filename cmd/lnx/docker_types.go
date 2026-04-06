package main

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"time"
)

const (
	dockerContainerCreated = "created"
	dockerContainerRunning = "running"
	dockerContainerExited  = "exited"
)

type dockerExecProcessConfig struct {
	Entrypoint   []string `json:"entrypoint,omitempty"`
	Env          []string `json:"env,omitempty"`
	Tty          bool     `json:"tty,omitempty"`
	WorkingDir   string   `json:"working_dir,omitempty"`
	AttachStdin  bool     `json:"attach_stdin,omitempty"`
	AttachStdout bool     `json:"attach_stdout,omitempty"`
	AttachStderr bool     `json:"attach_stderr,omitempty"`
}

type dockerImageConfig struct {
	Env        []string          `json:"env,omitempty"`
	Cmd        []string          `json:"cmd,omitempty"`
	Entrypoint []string          `json:"entrypoint,omitempty"`
	WorkingDir string            `json:"working_dir,omitempty"`
	Exposed    map[string]uint16 `json:"exposed,omitempty"`
}

type dockerImageMetadata struct {
	Reference      string            `json:"reference"`
	CanonicalRef   string            `json:"canonical_ref"`
	Digest         string            `json:"digest"`
	ManifestDigest string            `json:"manifest_digest"`
	ParentDigest   string            `json:"parent_digest,omitempty"`
	Layers         []string          `json:"layers"`
	Config         dockerImageConfig `json:"config"`
	CreatedAt      time.Time         `json:"created_at"`
	LastUsedAt     time.Time         `json:"last_used_at"`
}

type dockerContainerMetadata struct {
	ID          string            `json:"id"`
	Name        string            `json:"name,omitempty"`
	Image       string            `json:"image"`
	ImageDigest string            `json:"image_digest"`
	Instance    string            `json:"instance"`
	CreatedAt   time.Time         `json:"created_at"`
	StartedAt   *time.Time        `json:"started_at,omitempty"`
	FinishedAt  *time.Time        `json:"finished_at,omitempty"`
	State       string            `json:"state"`
	ExitCode    *int              `json:"exit_code,omitempty"`
	Pid         int               `json:"pid,omitempty"`
	WorkDir     string            `json:"work_dir,omitempty"`
	Env         []string          `json:"env,omitempty"`
	Entrypoint  []string          `json:"entrypoint,omitempty"`
	Cmd         []string          `json:"cmd,omitempty"`
	Tty         bool              `json:"tty"`
	OpenStdin   bool              `json:"open_stdin,omitempty"`
	AutoRemove  bool              `json:"auto_remove,omitempty"`
	Labels      map[string]string `json:"labels,omitempty"`
}

type dockerExecMetadata struct {
	ID            string                  `json:"id"`
	ContainerID   string                  `json:"container_id"`
	Instance      string                  `json:"instance"`
	CreatedAt     time.Time               `json:"created_at"`
	StartedAt     *time.Time              `json:"started_at,omitempty"`
	FinishedAt    *time.Time              `json:"finished_at,omitempty"`
	ExitCode      *int                    `json:"exit_code,omitempty"`
	Running       bool                    `json:"running,omitempty"`
	Pid           int                     `json:"pid,omitempty"`
	ProcessConfig dockerExecProcessConfig `json:"process_config"`
}

func dockerBaseDir() string {
	return filepath.Join(lnxBase(), "docker")
}

func dockerImagesDir() string {
	return filepath.Join(dockerBaseDir(), "images")
}

func dockerBlobsDir() string {
	return filepath.Join(dockerBaseDir(), "blobs", "sha256")
}

func dockerContainersDir() string {
	return filepath.Join(dockerBaseDir(), "containers")
}

func dockerExecsDir() string {
	return filepath.Join(dockerBaseDir(), "execs")
}

func dockerWaitsDir() string {
	return filepath.Join(dockerBaseDir(), "waits")
}

func dockerImageDir(digest string) string {
	return filepath.Join(dockerImagesDir(), sanitizeDigest(digest))
}

func dockerImageMetadataPath(digest string) string {
	return filepath.Join(dockerImageDir(digest), "image.json")
}

func dockerImageRootfsPath(digest string) string {
	return filepath.Join(dockerImageDir(digest), "rootfs.ext4")
}

func dockerImageLayersPath(digest string) string {
	return filepath.Join(dockerImageDir(digest), "layers.txt")
}

func dockerContainerDir(id string) string {
	return filepath.Join(dockerContainersDir(), id)
}

func dockerContainerMetadataPath(id string) string {
	return filepath.Join(dockerContainerDir(id), "container.json")
}

func dockerContainerLogPath(id string) string {
	return filepath.Join(dockerContainerDir(id), "container.log")
}

func dockerContainerInstanceName(id string) string {
	if len(id) > 12 {
		id = id[:12]
	}
	return "docker-" + id
}

func dockerContainerInstanceDir(id string) string {
	return filepath.Join(lnxBase(), "instances", dockerContainerInstanceName(id))
}

func dockerContainerRootfsPath(id string) string {
	return filepath.Join(dockerContainerInstanceDir(id), "rootfs.ext4")
}

func dockerExecDir(id string) string {
	return filepath.Join(dockerExecsDir(), id)
}

func dockerExecMetadataPath(id string) string {
	return filepath.Join(dockerExecDir(id), "exec.json")
}

func dockerWaitResultPath(id string) string {
	return filepath.Join(dockerWaitsDir(), id+".json")
}

func ensureDockerDirs() error {
	for _, dir := range []string{
		dockerBaseDir(),
		dockerImagesDir(),
		dockerBlobsDir(),
		dockerContainersDir(),
		dockerExecsDir(),
		dockerWaitsDir(),
	} {
		if err := os.MkdirAll(dir, 0755); err != nil {
			return err
		}
	}
	return nil
}

func sanitizeDigest(digest string) string {
	return filepath.Base(filepath.Clean(strings.ReplaceAll(digest, ":", "_")))
}

func newDockerID() (string, error) {
	var buf [32]byte
	if _, err := rand.Read(buf[:]); err != nil {
		return "", err
	}
	return hex.EncodeToString(buf[:]), nil
}

func loadDockerContainer(id string) (*dockerContainerMetadata, error) {
	var meta dockerContainerMetadata
	if err := readJSONFile(dockerContainerMetadataPath(id), &meta); err != nil {
		return nil, err
	}
	return &meta, nil
}

func saveDockerContainer(meta *dockerContainerMetadata) error {
	return writeJSONFile(dockerContainerMetadataPath(meta.ID), meta)
}

func loadDockerImage(digest string) (*dockerImageMetadata, error) {
	var meta dockerImageMetadata
	if err := readJSONFile(dockerImageMetadataPath(digest), &meta); err != nil {
		return nil, err
	}
	return &meta, nil
}

func saveDockerImage(meta *dockerImageMetadata) error {
	return writeJSONFile(dockerImageMetadataPath(meta.Digest), meta)
}

func loadDockerExec(id string) (*dockerExecMetadata, error) {
	var meta dockerExecMetadata
	if err := readJSONFile(dockerExecMetadataPath(id), &meta); err != nil {
		return nil, err
	}
	return &meta, nil
}

func saveDockerExec(meta *dockerExecMetadata) error {
	return writeJSONFile(dockerExecMetadataPath(meta.ID), meta)
}

func saveDockerWaitResult(id string, statusCode int) error {
	return writeJSONFile(dockerWaitResultPath(id), map[string]int{"status_code": statusCode})
}

func loadDockerWaitResult(id string) (int, error) {
	var result struct {
		StatusCode int `json:"status_code"`
	}
	if err := readJSONFile(dockerWaitResultPath(id), &result); err != nil {
		return 0, err
	}
	return result.StatusCode, nil
}

func writeJSONFile(path string, v any) error {
	if err := os.MkdirAll(filepath.Dir(path), 0755); err != nil {
		return err
	}
	data, err := json.MarshalIndent(v, "", "  ")
	if err != nil {
		return err
	}
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, data, 0644); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}

func readJSONFile(path string, v any) error {
	data, err := os.ReadFile(path)
	if err != nil {
		return err
	}
	return json.Unmarshal(data, v)
}
