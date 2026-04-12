package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"net"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"syscall"
	"time"

	"github.com/semistrict/lnx"
	"github.com/spf13/cobra"
)

// findDefaultRootfs returns the path to a default rootfs to clone from.
// Tries the un-prefixed "default" first (host's rootfs, always available
// via the ~/.lnx share), then the qualified name for this nesting level.
func findDefaultRootfs() string {
	names := []string{"default"}
	if qualified := qualifyName("default"); qualified != "default" {
		names = append(names, qualified)
	}
	for _, name := range names {
		p := filepath.Join(imagesDirFor(name), "rootfs.ext4")
		if _, err := os.Stat(p); err == nil {
			return p
		}
	}
	return ""
}

// qualifyName prefixes an instance name with LNX_PARENT when nested.
func qualifyName(name string) string {
	parent := os.Getenv("LNX_PARENT")
	if parent == "" {
		return name
	}
	return parent + "." + name
}

var instanceCmd = &cobra.Command{
	Use:   "instance",
	Short: "Manage VM instances",
}

var instanceListCmd = &cobra.Command{
	Use:   "list",
	Short: "List all instances",
	Args:  cobra.NoArgs,
	RunE:  runInstanceList,
}

var cloneCmd = &cobra.Command{
	Use:   "clone <name>",
	Short: "Clone the current source instance into a new instance",
	Args:  cobra.ExactArgs(1),
	RunE:  runInstanceClone,
}

var instanceInitCmd = &cobra.Command{
	Use:   "init <name>",
	Short: "Initialize a new instance from source files",
	Long: `Initialize a new instance. If --kernel and --rootfs are provided, copies them.
Otherwise, if the default instance exists, clones its rootfs via APFS clonefile.`,
	Args: cobra.ExactArgs(1),
	RunE: runInstanceInit,
}

var instanceDeleteCmd = &cobra.Command{
	Use:               "delete <name>",
	Short:             "Delete an instance",
	Args:              cobra.ExactArgs(1),
	RunE:              runInstanceDelete,
	ValidArgsFunction: completeInstanceNames,
}

var (
	instInitKernel  string
	instInitRootfs  string
	cloneCheckpoint string
)

func init() {
	instanceInitCmd.Flags().StringVar(&instInitKernel, "kernel", "", "path to kernel Image (copies to shared location)")
	instanceInitCmd.Flags().StringVar(&instInitRootfs, "rootfs", "", "path to rootfs ext4 image")
	cloneCmd.Flags().StringVar(&cloneCheckpoint, "checkpoint", "", "clone from an existing or newly created named checkpoint of the source instance")

	instanceCmd.AddCommand(instanceListCmd)
	instanceCmd.AddCommand(instanceInitCmd)
	instanceCmd.AddCommand(instanceDeleteCmd)
	rootCmd.AddCommand(instanceCmd)
	rootCmd.AddCommand(cloneCmd)
}

func runInstanceList(cmd *cobra.Command, args []string) error {
	base := lnxBase()
	seen := make(map[string]bool)

	// Collect instance names from both images/ (new layout) and instances/ (legacy).
	for _, subdir := range []string{"images", "instances"} {
		dir := filepath.Join(base, subdir)
		entries, err := os.ReadDir(dir)
		if err != nil {
			continue
		}
		for _, e := range entries {
			if !e.IsDir() || seen[e.Name()] {
				continue
			}
			// Check for rootfs in the appropriate location.
			if _, err := os.Stat(resolveRootfsPathFor(e.Name())); err == nil {
				seen[e.Name()] = true
			}
		}
	}

	var instances []string
	for name := range seen {
		instances = append(instances, name)
	}
	sort.Strings(instances)

	if len(instances) == 0 {
		fmt.Println("no instances")
		return nil
	}

	t := newTable("NAME", "STATUS")
	for _, name := range instances {
		status := dimStyle.Render("stopped")
		sockPath := filepath.Join(instanceDirFor(name), "status.sock")
		conn, err := net.DialTimeout("unix", sockPath, 500*time.Millisecond)
		if err == nil {
			conn.Close()
			status = greenStyle.Render("running")
		}
		t.Row(name, status)
	}
	fmt.Println(t)
	return nil
}

func runInstanceClone(cmd *cobra.Command, args []string) error {
	name := qualifyName(args[0])
	if name == "default" {
		return fmt.Errorf("cannot clone into instance named 'default' (use 'lnx init' instead)")
	}

	// Check if instance already exists (images or instances dir).
	imgDir := imagesDirFor(name)
	instDir := instanceDirFor(name)
	if _, err := os.Stat(imgDir); err == nil {
		return fmt.Errorf("instance %q already exists", name)
	}
	if _, err := os.Stat(instDir); err == nil {
		return fmt.Errorf("instance %q already exists", name)
	}

	sourceName := qualifiedInstanceName()
	checkpointName := cloneCheckpoint
	sourceImgDir, sourceInstDir, sourceResolvedName, err := resolveCloneSourceDirs(sourceName)
	if err != nil {
		return err
	}

	if err := os.MkdirAll(imgDir, 0755); err != nil {
		return fmt.Errorf("create images dir: %w", err)
	}
	if err := os.MkdirAll(instDir, 0755); err != nil {
		_ = os.RemoveAll(imgDir)
		return fmt.Errorf("create instance dir: %w", err)
	}

	checkpointPath, err := checkpointPathForClone(sourceImgDir, sourceResolvedName, checkpointName)
	if err != nil {
		_ = os.RemoveAll(imgDir)
		_ = os.RemoveAll(instDir)
		return err
	}

	if err := cloneRootfs(checkpointPath, filepath.Join(imgDir, "rootfs.ext4")); err != nil {
		_ = os.RemoveAll(imgDir)
		_ = os.RemoveAll(instDir)
		return fmt.Errorf("clone rootfs: %w", err)
	}
	if err := cloneInstanceMetadata(sourceInstDir, instDir); err != nil {
		_ = os.RemoveAll(imgDir)
		_ = os.RemoveAll(instDir)
		return fmt.Errorf("clone metadata: %w", err)
	}

	if checkpointName == "" {
		fmt.Printf("created instance %q from %q\n", name, sourceResolvedName)
	} else {
		fmt.Printf("created instance %q from %q\n", name, sourceResolvedName+":"+checkpointName)
	}
	return nil
}

func runInstanceInit(cmd *cobra.Command, args []string) error {
	name := qualifyName(args[0])
	imgDir := imagesDirFor(name)

	if _, err := os.Stat(filepath.Join(imgDir, "rootfs.ext4")); err == nil {
		return fmt.Errorf("instance %q already exists", name)
	}
	// Also check legacy location.
	if _, err := os.Stat(filepath.Join(instanceDirFor(name), "rootfs.ext4")); err == nil {
		return fmt.Errorf("instance %q already exists", name)
	}

	if err := os.MkdirAll(imgDir, 0755); err != nil {
		return fmt.Errorf("create images dir: %w", err)
	}

	// Copy kernel to shared location if provided.
	if instInitKernel != "" {
		base := lnxBase()
		os.MkdirAll(base, 0755)
		kernelDest := filepath.Join(base, "vmlinuz")
		if err := copyFile(kernelDest, instInitKernel); err != nil {
			return fmt.Errorf("copy kernel: %w", err)
		}
		fmt.Printf("  kernel: %s\n", kernelDest)
	}

	rootfsDest := filepath.Join(imgDir, "rootfs.ext4")

	if instInitRootfs != "" {
		// Copy from provided rootfs file.
		if err := copyFile(rootfsDest, instInitRootfs); err != nil {
			return fmt.Errorf("copy rootfs: %w", err)
		}
	} else {
		// Clone from default instance if it exists.
		defaultRootfs := findDefaultRootfs()
		if defaultRootfs == "" {
			os.RemoveAll(imgDir)
			return fmt.Errorf("no --rootfs specified and no default rootfs found — run 'lnx init' first")
		}
		if err := cloneRootfs(defaultRootfs, rootfsDest); err != nil {
			os.RemoveAll(imgDir)
			return fmt.Errorf("clone rootfs from default: %w", err)
		}
	}
	fmt.Printf("  rootfs: %s\n", rootfsDest)

	fmt.Printf("instance %q initialized\n", name)
	return nil
}

func runInstanceDelete(cmd *cobra.Command, args []string) error {
	name := qualifyName(args[0])
	if name == "default" {
		return fmt.Errorf("cannot delete the default instance")
	}

	imgDir := imagesDirFor(name)
	instDir := instanceDirFor(name)

	// Check that the instance exists (in either location).
	imgExists := false
	instExists := false
	if _, err := os.Stat(imgDir); err == nil {
		imgExists = true
	}
	if _, err := os.Stat(instDir); err == nil {
		instExists = true
	}
	if !imgExists && !instExists {
		return fmt.Errorf("instance %q does not exist", name)
	}

	// Refuse if the VM is running.
	sockPath := filepath.Join(instDir, "status.sock")
	conn, err := net.DialTimeout("unix", sockPath, 500*time.Millisecond)
	if err == nil {
		conn.Close()
		return fmt.Errorf("instance %q is running — stop it first", name)
	}

	// Check for rootfs lock.
	rootfs := resolveRootfsPathFor(name)
	lockPath := rootfs + ".lock"
	if f, err := os.OpenFile(lockPath, os.O_RDWR, 0); err == nil {
		defer f.Close()
		if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
			return fmt.Errorf("instance %q rootfs is locked — another process may be using it", name)
		}
		syscall.Flock(int(f.Fd()), syscall.LOCK_UN)
	}

	if imgExists {
		if err := os.RemoveAll(imgDir); err != nil {
			return fmt.Errorf("delete instance images: %w", err)
		}
	}
	if instExists {
		if err := os.RemoveAll(instDir); err != nil {
			return fmt.Errorf("delete instance: %w", err)
		}
	}

	fmt.Printf("deleted instance %q\n", name)
	return nil
}

// resolveCloneSourceDirs returns the images dir, instances dir, and resolved name
// for the clone source instance.
func resolveCloneSourceDirs(source string) (imgDir, instDir, resolvedName string, err error) {
	var candidates []string
	if source == "default" {
		candidates = append(candidates, "default")
		if qualified := qualifyName("default"); qualified != "default" {
			candidates = append(candidates, qualified)
		}
	} else {
		candidates = append(candidates, qualifyName(source))
	}

	for _, name := range candidates {
		rootfs := resolveRootfsPathFor(name)
		if _, err := os.Stat(rootfs); err == nil {
			return filepath.Dir(rootfs), instanceDirFor(name), name, nil
		}
	}
	if source == "default" {
		return "", "", "", fmt.Errorf("no default rootfs found — run 'lnx init' first")
	}
	return "", "", "", fmt.Errorf("instance %q does not exist", qualifyName(source))
}

func checkpointPathForClone(sourceDir, sourceName, checkpointName string) (string, error) {
	if checkpointName != "" {
		return resolveNamedCheckpoint(sourceDir, checkpointName)
	}
	return createInstanceCheckpoint(sourceDir, sourceName, "")
}

func resolveNamedCheckpoint(sourceDir, checkpointName string) (string, error) {
	dir := filepath.Join(sourceDir, "checkpoints")
	candidates := []string{checkpointName}
	if filepath.Ext(checkpointName) != ".ext4" {
		candidates = append(candidates, checkpointName+".ext4")
	}
	for _, candidate := range candidates {
		path := filepath.Join(dir, candidate)
		if _, err := os.Stat(path); err == nil {
			return path, nil
		}
	}
	return "", fmt.Errorf("checkpoint %q not found", checkpointName)
}

func createInstanceCheckpoint(sourceImgDir, sourceName, checkpointName string) (string, error) {
	if isInstanceRunning(sourceName) {
		return createCheckpointViaAPI(sourceName, checkpointName)
	}

	rootfsPath := filepath.Join(sourceImgDir, "rootfs.ext4")
	lock, err := lnx.LockRootfs(rootfsPath)
	if err != nil {
		return "", fmt.Errorf("lock rootfs: %w", err)
	}
	defer lock.Unlock()

	return lnx.CreateCheckpoint(rootfsPath, filepath.Join(sourceImgDir, "checkpoints"), checkpointName)
}

func createCheckpointViaAPI(instanceName, checkpointName string) (string, error) {
	body, err := json.Marshal(map[string]string{"name": checkpointName})
	if err != nil {
		return "", fmt.Errorf("marshal checkpoint request: %w", err)
	}

	resp, err := apiClientFor(instanceName).Post("http://localhost/checkpoint", "application/json", bytes.NewReader(body))
	if err != nil {
		if isNoVM(err) {
			return "", noVMError()
		}
		return "", err
	}
	defer resp.Body.Close()

	if resp.StatusCode/100 != 2 {
		data, _ := io.ReadAll(resp.Body)
		msg := strings.TrimSpace(string(data))
		if msg == "" {
			msg = resp.Status
		}
		return "", errors.New(msg)
	}

	var payload struct {
		Path string `json:"path"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&payload); err != nil {
		return "", fmt.Errorf("decode checkpoint response: %w", err)
	}
	if payload.Path == "" {
		return "", fmt.Errorf("checkpoint response missing path")
	}
	return payload.Path, nil
}

func isInstanceRunning(name string) bool {
	conn, err := net.DialTimeout("unix", filepath.Join(lnxBase(), "instances", name, "status.sock"), 500*time.Millisecond)
	if err != nil {
		return false
	}
	_ = conn.Close()
	return true
}

func cloneInstanceMetadata(srcDir, dstDir string) error {
	return filepath.WalkDir(srcDir, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if path == srcDir {
			return nil
		}

		rel, err := filepath.Rel(srcDir, path)
		if err != nil {
			return err
		}
		if shouldSkipClonedMetadata(rel, d) {
			if d.IsDir() {
				return filepath.SkipDir
			}
			return nil
		}

		dstPath := filepath.Join(dstDir, rel)
		if d.IsDir() {
			return os.MkdirAll(dstPath, 0755)
		}
		if d.Type()&os.ModeSymlink != 0 {
			target, err := os.Readlink(path)
			if err != nil {
				return err
			}
			return os.Symlink(target, dstPath)
		}
		if !d.Type().IsRegular() {
			return nil
		}
		return copyFile(dstPath, path)
	})
}

func shouldSkipClonedMetadata(rel string, d fs.DirEntry) bool {
	base := filepath.Base(rel)
	if rel == "rootfs.ext4" || rel == "criu.ext4" || base == "checkpoints" {
		return true
	}
	switch base {
	case "status.sock", "error.log", "serial.log", "lnx.log", "initramfs.cpio", "swap.img",
		"rootfs.ext4.lock", "rootfs.ext4.pid", "firecracker.sock", "vsock", "hibernated":
		return true
	}
	return strings.HasPrefix(base, "vsock_")
}

// completeInstanceNames provides shell completion for instance names.
func completeInstanceNames(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
	if len(args) > 0 {
		return nil, cobra.ShellCompDirectiveNoFileComp
	}
	base := lnxBase()
	seen := make(map[string]bool)
	for _, subdir := range []string{"images", "instances"} {
		entries, err := os.ReadDir(filepath.Join(base, subdir))
		if err != nil {
			continue
		}
		for _, e := range entries {
			if e.IsDir() {
				seen[e.Name()] = true
			}
		}
	}
	var names []string
	for name := range seen {
		names = append(names, name)
	}
	sort.Strings(names)
	return names, cobra.ShellCompDirectiveNoFileComp
}
