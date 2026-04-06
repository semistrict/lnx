package main

import (
	"fmt"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"time"

	"github.com/spf13/cobra"
)

var dockerSocketPath string
var dockerForeground bool

var dockerServeCmd = &cobra.Command{
	Use:   "serve-docker",
	Short: "Serve a Docker-compatible API on a Unix socket",
	Args:  cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		return runServeDockerBackground()
	},
}

var dockerServeHelperCmd = &cobra.Command{
	Use:    "_serve_docker",
	Short:  "Run the Docker-compatible API server in the background",
	Hidden: true,
	Args:   cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		parent, ctx, err := daemonizeCurrentProcess()
		if err != nil {
			return err
		}
		if parent {
			return nil
		}
		defer func() { _ = ctx.Release() }()

		return runServeDocker()
	},
}

func init() {
	defaultSock := filepath.Join(lnxBase(), "docker.sock")
	dockerServeCmd.Flags().StringVar(&dockerSocketPath, "socket", defaultSock, "Unix socket path to serve the Docker API on")
	dockerServeCmd.Flags().BoolVar(&dockerForeground, "foreground", false, "Run in the foreground")
	_ = dockerServeCmd.Flags().MarkHidden("foreground")
	rootCmd.AddCommand(dockerServeCmd)

	dockerServeHelperCmd.Flags().StringVar(&dockerSocketPath, "socket", defaultSock, "Unix socket path to serve the Docker API on")
	rootCmd.AddCommand(dockerServeHelperCmd)
}

func runServeDockerBackground() error {
	if dockerForeground {
		return runServeDocker()
	}
	if err := spawnDockerServeHelper(); err != nil {
		return err
	}
	if err := waitForDockerSocket(dockerSocketPath, 10*time.Second); err != nil {
		return err
	}
	fmt.Printf("Docker API listening on unix://%s\n", dockerSocketPath)
	return nil
}

func spawnDockerServeHelper() error {
	self, err := os.Executable()
	if err != nil {
		return fmt.Errorf("find executable: %w", err)
	}

	cmd := exec.Command(self, "_serve_docker", "--instance", instanceName, "--socket", dockerSocketPath)
	configureBackgroundCommand(cmd)
	if err := cmd.Start(); err != nil {
		return fmt.Errorf("start Docker server helper: %w", err)
	}
	return cmd.Process.Release()
}

func waitForDockerSocket(path string, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		if _, err := os.Stat(path); err == nil {
			return nil
		}
		time.Sleep(50 * time.Millisecond)
	}
	return fmt.Errorf("timed out waiting for Docker socket %s", path)
}

func runServeDocker() error {
	if dockerSocketPath == "" {
		return fmt.Errorf("--socket is required")
	}
	if err := ensureDockerDirs(); err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(dockerSocketPath), 0755); err != nil {
		return err
	}
	_ = os.Remove(dockerSocketPath)

	ln, err := net.Listen("unix", dockerSocketPath)
	if err != nil {
		return fmt.Errorf("listen docker socket: %w", err)
	}
	defer func() {
		_ = ln.Close()
		_ = os.Remove(dockerSocketPath)
	}()

	srv := &http.Server{Handler: newDockerMux()}
	return srv.Serve(ln)
}
