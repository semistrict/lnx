package main

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"net"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"time"

	"github.com/semistrict/lnx/internal/lnxoci"
	"github.com/spf13/cobra"
)

var dockerCmd = &cobra.Command{
	Use:   "docker",
	Short: "Run OCI container images in lnx VMs",
}

var dockerRunCmd = &cobra.Command{
	Use:                "run IMAGE[:TAG] [COMMAND [ARGS...]]",
	Short:              "Pull and run an OCI container image",
	Args:               cobra.MinimumNArgs(1),
	RunE:               runDockerRun,
	DisableFlagParsing: true,
}

var dockerPsCmd = &cobra.Command{
	Use:   "ps",
	Short: "List containers",
	Args:  cobra.NoArgs,
	RunE:  runDockerPs,
}

func init() {
	dockerCmd.AddCommand(dockerRunCmd)
	dockerCmd.AddCommand(dockerPsCmd)
	rootCmd.AddCommand(dockerCmd)
}

// containerMeta is persisted as container.json alongside each container's rootfs.
type containerMeta struct {
	ID      string        `json:"id"`
	Image   string        `json:"image"`
	Command []string      `json:"command"`
	Created time.Time     `json:"created"`
	Ports   []portMapping `json:"ports,omitempty"`
}

// portMapping is a resolved host→guest port binding.
type portMapping struct {
	Host  uint16 `json:"host"`
	Guest uint16 `json:"guest"`
}

// imageMeta is persisted as image.json alongside a base image rootfs.
type imageMeta struct {
	ExposedPorts []uint16 `json:"exposed_ports,omitempty"`
}

// dockerImagesDir returns ~/.lnx/docker/images — the base OCI image store.
func dockerImagesDir() string {
	return filepath.Join(lnxBase(), "docker", "images")
}

// dockerImageDirFor returns the directory for a specific OCI base image.
func dockerImageDirFor(name string) string {
	return filepath.Join(dockerImagesDir(), name)
}

// dockerContainersDir returns ~/.lnx/docker/containers.
func dockerContainersDir() string {
	return filepath.Join(lnxBase(), "docker", "containers")
}

// dockerContainerDirFor returns the images directory for a container instance.
// imagesDirFor resolves to this path for docker container IDs.
func dockerContainerDirFor(id string) string {
	return filepath.Join(dockerContainersDir(), id)
}

func runDockerRun(cmd *cobra.Command, args []string) error {
	// DisableFlagParsing means all args are raw. Strip Docker-compatible flags
	// that appear before the image name.
	var portSpecs []string
	var publishAll bool
	for len(args) > 0 {
		a := args[0]
		switch {
		case a == "-i" || a == "-t" || a == "-it" || a == "-ti" ||
			a == "--interactive" || a == "--tty":
			args = args[1:]
		case a == "-P" || a == "--publish-all":
			publishAll = true
			args = args[1:]
		case a == "-p" || a == "--publish":
			if len(args) < 2 {
				return fmt.Errorf("flag %q requires an argument", a)
			}
			portSpecs = append(portSpecs, args[1])
			args = args[2:]
		case strings.HasPrefix(a, "-p=") || strings.HasPrefix(a, "--publish="):
			portSpecs = append(portSpecs, strings.SplitN(a, "=", 2)[1])
			args = args[1:]
		default:
			goto flagsDone
		}
	}
flagsDone:
	if len(args) == 0 {
		return fmt.Errorf("requires image name")
	}

	imageRef := args[0]
	if !strings.Contains(imageRef, ":") {
		imageRef += ":latest"
	}
	cmdArgs := args[1:] // optional command override

	inst := lnxoci.SlugFromRef(imageRef)

	baseRootfs, err := ensureOCIRootfs(imageRef)
	if err != nil {
		return err
	}

	// Resolve -p specs into (host, guest) pairs.
	var wantMappings []portMapping
	for _, spec := range portSpecs {
		h, g, err := parsePortMapping(spec)
		if err != nil {
			return fmt.Errorf("invalid -p %q: %w", spec, err)
		}
		wantMappings = append(wantMappings, portMapping{Host: h, Guest: g})
	}
	// -P: add all ports declared by the image.
	if publishAll {
		imgMeta, _ := readImageMeta(inst)
		for _, p := range imgMeta.ExposedPorts {
			wantMappings = append(wantMappings, portMapping{Host: 0, Guest: p})
		}
	}

	// Resolve the run command before creating the container so we can persist it.
	runArgs := cmdArgs
	if len(runArgs) == 0 {
		runArgs = readDefaultCmd(inst)
	}
	if len(runArgs) == 0 {
		runArgs = []string{"/bin/sh"}
	}

	// Create an ephemeral container: reflink clone of the base image rootfs.
	containerID, err := newContainerID(inst)
	if err != nil {
		return fmt.Errorf("generate container ID: %w", err)
	}
	containerDir := dockerContainerDirFor(containerID)
	if err := os.MkdirAll(containerDir, 0755); err != nil {
		return fmt.Errorf("create container dir: %w", err)
	}
	containerRootfs := filepath.Join(containerDir, "rootfs.ext4")
	if err := cloneRootfs(baseRootfs, containerRootfs); err != nil {
		os.RemoveAll(containerDir)
		return fmt.Errorf("clone container rootfs: %w", err)
	}

	meta := containerMeta{
		ID:      containerID,
		Image:   imageRef,
		Command: runArgs,
		Created: time.Now(),
	}
	if err := writeContainerMeta(containerDir, meta); err != nil {
		os.RemoveAll(containerDir)
		return fmt.Errorf("write container metadata: %w", err)
	}

	defer func() {
		os.RemoveAll(containerDir)
		os.RemoveAll(instanceDirFor(containerID))
	}()

	instanceName = containerID
	instanceFlag = true

	// Start the VM so we can register port mappings before exec.
	if err := ensureVMRunning(); err != nil {
		return err
	}

	// Register port mappings with the running daemon.
	if len(wantMappings) > 0 {
		var resolved []portMapping
		for _, m := range wantMappings {
			resp, err := exposeHostPort(containerID, m.Guest, m.Host, true)
			if err != nil {
				return fmt.Errorf("expose port %d: %w", m.Guest, err)
			}
			resolved = append(resolved, portMapping{Host: resp.HostPort, Guest: m.Guest})
		}
		// Persist the resolved host ports so docker ps can show them.
		meta.Ports = resolved
		_ = writeContainerMeta(containerDir, meta)
	}

	exitCode, err := runVM(runArgs)
	if err != nil {
		return err
	}
	os.Exit(exitCode)
	return nil
}

func runDockerPs(cmd *cobra.Command, args []string) error {
	entries, err := os.ReadDir(dockerContainersDir())
	if err != nil {
		if os.IsNotExist(err) {
			return nil
		}
		return fmt.Errorf("read containers dir: %w", err)
	}

	type row struct {
		meta    containerMeta
		running bool
	}
	var rows []row
	for _, e := range entries {
		if !e.IsDir() {
			continue
		}
		meta, err := readContainerMeta(filepath.Join(dockerContainersDir(), e.Name()))
		if err != nil {
			continue
		}
		sock := filepath.Join(instanceDirFor(meta.ID), "status.sock")
		running := false
		if c, err := net.DialTimeout("unix", sock, 200*time.Millisecond); err == nil {
			c.Close()
			running = true
		}
		rows = append(rows, row{meta, running})
	}

	sort.Slice(rows, func(i, j int) bool {
		return rows[i].meta.Created.Before(rows[j].meta.Created)
	})

	t := newTable("CONTAINER ID", "IMAGE", "COMMAND", "CREATED", "STATUS", "PORTS")
	for _, r := range rows {
		status := dimStyle.Render("Exited")
		if r.running {
			status = greenStyle.Render("Up " + humanDuration(time.Since(r.meta.Created)))
		}
		cmdStr := shellJoin(r.meta.Command)
		if len(cmdStr) > 20 {
			cmdStr = cmdStr[:20] + "…"
		}
		var portStrs []string
		for _, p := range r.meta.Ports {
			portStrs = append(portStrs, fmt.Sprintf("0.0.0.0:%d->%d/tcp", p.Host, p.Guest))
		}
		t.Row(
			r.meta.ID,
			r.meta.Image,
			`"`+cmdStr+`"`,
			humanDuration(time.Since(r.meta.Created))+" ago",
			status,
			strings.Join(portStrs, ", "),
		)
	}
	fmt.Println(t)
	return nil
}

// parsePortMapping parses a Docker -p spec: [hostPort:]guestPort[/proto].
// Returns host=0 if no host port is specified (ephemeral).
func parsePortMapping(s string) (host, guest uint16, err error) {
	// Strip optional /proto suffix.
	if i := strings.LastIndex(s, "/"); i >= 0 {
		s = s[:i]
	}
	parts := strings.SplitN(s, ":", 2)
	if len(parts) == 1 {
		n, err := strconv.ParseUint(parts[0], 10, 16)
		if err != nil || n == 0 {
			return 0, 0, fmt.Errorf("invalid port %q", parts[0])
		}
		return 0, uint16(n), nil
	}
	h, err := strconv.ParseUint(parts[0], 10, 16)
	if err != nil || h == 0 {
		return 0, 0, fmt.Errorf("invalid host port %q", parts[0])
	}
	g, err := strconv.ParseUint(parts[1], 10, 16)
	if err != nil || g == 0 {
		return 0, 0, fmt.Errorf("invalid guest port %q", parts[1])
	}
	return uint16(h), uint16(g), nil
}

func writeContainerMeta(dir string, meta containerMeta) error {
	data, err := json.MarshalIndent(meta, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(dir, "container.json"), data, 0644)
}

func readContainerMeta(dir string) (containerMeta, error) {
	data, err := os.ReadFile(filepath.Join(dir, "container.json"))
	if err != nil {
		return containerMeta{}, err
	}
	var meta containerMeta
	if err := json.Unmarshal(data, &meta); err != nil {
		return containerMeta{}, err
	}
	return meta, nil
}

func writeImageMeta(inst string, meta imageMeta) {
	data, err := json.MarshalIndent(meta, "", "  ")
	if err != nil {
		return
	}
	_ = os.WriteFile(filepath.Join(dockerImageDirFor(inst), "image.json"), data, 0644)
}

func readImageMeta(inst string) (imageMeta, error) {
	data, err := os.ReadFile(filepath.Join(dockerImageDirFor(inst), "image.json"))
	if err != nil {
		return imageMeta{}, err
	}
	var meta imageMeta
	if err := json.Unmarshal(data, &meta); err != nil {
		return imageMeta{}, err
	}
	return meta, nil
}

// humanDuration formats a duration in Docker-style human-readable form.
func humanDuration(d time.Duration) string {
	d = d.Round(time.Second)
	switch {
	case d < time.Minute:
		return fmt.Sprintf("%d seconds", int(d.Seconds()))
	case d < time.Hour:
		m := int(d.Minutes())
		if m == 1 {
			return "1 minute"
		}
		return fmt.Sprintf("%d minutes", m)
	case d < 24*time.Hour:
		h := int(d.Hours())
		if h == 1 {
			return "1 hour"
		}
		return fmt.Sprintf("%d hours", h)
	default:
		days := int(d.Hours() / 24)
		if days == 1 {
			return "1 day"
		}
		return fmt.Sprintf("%d days", days)
	}
}

// shellJoin joins args into a display string, quoting args that contain spaces.
func shellJoin(args []string) string {
	parts := make([]string, len(args))
	for i, a := range args {
		if strings.ContainsAny(a, " \t\"'") {
			parts[i] = `"` + strings.ReplaceAll(a, `"`, `\"`) + `"`
		} else {
			parts[i] = a
		}
	}
	return strings.Join(parts, " ")
}

// newContainerID generates a slug-based container ID: <image-slug>-<6-hex-chars>.
func newContainerID(imageSlug string) (string, error) {
	b := make([]byte, 3)
	if _, err := rand.Read(b); err != nil {
		return "", err
	}
	return imageSlug + "-" + hex.EncodeToString(b), nil
}


// writeDefaultCmd saves the image's default command alongside the base image.
func writeDefaultCmd(inst string, cmd []string) {
	p := filepath.Join(dockerImageDirFor(inst), "cmd")
	_ = os.WriteFile(p, []byte(strings.Join(cmd, "\x00")), 0644)
}

// readDefaultCmd loads the saved default command for a base image.
func readDefaultCmd(inst string) []string {
	p := filepath.Join(dockerImageDirFor(inst), "cmd")
	data, err := os.ReadFile(p)
	if err != nil || len(data) == 0 {
		return nil
	}
	return strings.Split(string(data), "\x00")
}
