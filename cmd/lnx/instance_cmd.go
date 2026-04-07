package main

import (
	"fmt"
	"net"
	"os"
	"path/filepath"
	"syscall"
	"time"

	"github.com/spf13/cobra"
)

// findDefaultRootfs returns the path to a default rootfs to clone from.
// Tries the un-prefixed "default" first (host's rootfs, always available
// via the ~/.lnx share), then the qualified name for this nesting level.
func findDefaultRootfs() string {
	candidates := []string{
		filepath.Join(lnxBase(), "instances", "default", "rootfs.ext4"),
	}
	qualified := qualifyName("default")
	if qualified != "default" {
		candidates = append(candidates, filepath.Join(lnxBase(), "instances", qualified, "rootfs.ext4"))
	}
	for _, p := range candidates {
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

var instanceCreateCmd = &cobra.Command{
	Use:   "create <name>",
	Short: "Create a new instance by cloning default's rootfs",
	Args:  cobra.ExactArgs(1),
	RunE:  runInstanceCreate,
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
	Use:   "delete <name>",
	Short: "Delete an instance",
	Args:  cobra.ExactArgs(1),
	RunE:  runInstanceDelete,
}

var (
	instInitKernel string
	instInitRootfs string
)

func init() {
	instanceInitCmd.Flags().StringVar(&instInitKernel, "kernel", "", "path to kernel Image (copies to shared location)")
	instanceInitCmd.Flags().StringVar(&instInitRootfs, "rootfs", "", "path to rootfs ext4 image")

	instanceCmd.AddCommand(instanceListCmd)
	instanceCmd.AddCommand(instanceCreateCmd)
	instanceCmd.AddCommand(instanceInitCmd)
	instanceCmd.AddCommand(instanceDeleteCmd)
	rootCmd.AddCommand(instanceCmd)
}

func runInstanceList(cmd *cobra.Command, args []string) error {
	instancesDir := filepath.Join(lnxBase(), "instances")
	entries, err := os.ReadDir(instancesDir)
	if err != nil {
		if os.IsNotExist(err) {
			fmt.Println("no instances")
			return nil
		}
		return err
	}

	var instances []string
	for _, e := range entries {
		if !e.IsDir() {
			continue
		}
		// Only list directories that contain a rootfs.
		if _, err := os.Stat(filepath.Join(instancesDir, e.Name(), "rootfs.ext4")); err != nil {
			continue
		}
		instances = append(instances, e.Name())
	}

	if len(instances) == 0 {
		fmt.Println("no instances")
		return nil
	}

	t := newTable("NAME", "STATUS")
	for _, name := range instances {
		status := dimStyle.Render("stopped")
		sockPath := filepath.Join(instancesDir, name, "status.sock")
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

func runInstanceCreate(cmd *cobra.Command, args []string) error {
	name := qualifyName(args[0])
	if name == "default" {
		return fmt.Errorf("cannot create instance named 'default' (use 'lnx init' instead)")
	}

	dir := filepath.Join(lnxBase(), "instances", name)
	if _, err := os.Stat(dir); err == nil {
		return fmt.Errorf("instance %q already exists", name)
	}

	// Find the source rootfs: try the host's "default" first (always available
	// via ~/.lnx share), then fall back to the qualified name for this nesting level.
	defaultRootfs := findDefaultRootfs()
	if defaultRootfs == "" {
		return fmt.Errorf("no default rootfs found — run 'lnx init' first")
	}

	if err := os.MkdirAll(dir, 0755); err != nil {
		return fmt.Errorf("create instance dir: %w", err)
	}

	dst := filepath.Join(dir, "rootfs.ext4")
	if err := cloneRootfs(defaultRootfs, dst); err != nil {
		os.RemoveAll(dir)
		return fmt.Errorf("clone rootfs: %w", err)
	}

	fmt.Printf("created instance %q\n", name)
	return nil
}

func runInstanceInit(cmd *cobra.Command, args []string) error {
	name := qualifyName(args[0])
	dir := filepath.Join(lnxBase(), "instances", name)

	if _, err := os.Stat(filepath.Join(dir, "rootfs.ext4")); err == nil {
		return fmt.Errorf("instance %q already exists", name)
	}

	if err := os.MkdirAll(dir, 0755); err != nil {
		return fmt.Errorf("create instance dir: %w", err)
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

	rootfsDest := filepath.Join(dir, "rootfs.ext4")

	if instInitRootfs != "" {
		// Copy from provided rootfs file.
		if err := copyFile(rootfsDest, instInitRootfs); err != nil {
			return fmt.Errorf("copy rootfs: %w", err)
		}
	} else {
		// Clone from default instance if it exists.
		defaultRootfs := findDefaultRootfs()
		if defaultRootfs == "" {
			os.RemoveAll(dir)
			return fmt.Errorf("no --rootfs specified and no default rootfs found — run 'lnx init' first")
		}
		if err := cloneRootfs(defaultRootfs, rootfsDest); err != nil {
			os.RemoveAll(dir)
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

	dir := filepath.Join(lnxBase(), "instances", name)
	if _, err := os.Stat(dir); os.IsNotExist(err) {
		return fmt.Errorf("instance %q does not exist", name)
	}

	// Refuse if the VM is running.
	sockPath := filepath.Join(dir, "status.sock")
	conn, err := net.DialTimeout("unix", sockPath, 500*time.Millisecond)
	if err == nil {
		conn.Close()
		return fmt.Errorf("instance %q is running — stop it first", name)
	}

	// Check for rootfs lock.
	lockPath := filepath.Join(dir, "rootfs.ext4.lock")
	if f, err := os.OpenFile(lockPath, os.O_RDWR, 0); err == nil {
		defer f.Close()
		if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
			return fmt.Errorf("instance %q rootfs is locked — another process may be using it", name)
		}
		syscall.Flock(int(f.Fd()), syscall.LOCK_UN)
	}

	if err := os.RemoveAll(dir); err != nil {
		return fmt.Errorf("delete instance: %w", err)
	}

	fmt.Printf("deleted instance %q\n", name)
	return nil
}
