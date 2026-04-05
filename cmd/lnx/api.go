package main

import (
	"context"
	"fmt"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"time"
)

// apiClientFor returns an HTTP client that talks to a specific instance's unix socket.
func apiClientFor(name string) *http.Client {
	sockPath := filepath.Join(lnxBase(), "instances", name, "status.sock")
	return &http.Client{
		Transport: &http.Transport{
			DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
				return net.DialTimeout("unix", sockPath, 2*time.Second)
			},
		},
	}
}

// apiClient returns an HTTP client for the current --instance.
func apiClient() *http.Client {
	return apiClientFor(instanceName)
}

// runningInstances returns a list of instance names that have a reachable status.sock.
func runningInstances() []string {
	instancesDir := filepath.Join(lnxBase(), "instances")
	entries, err := os.ReadDir(instancesDir)
	if err != nil {
		return nil
	}

	var running []string
	for _, e := range entries {
		if !e.IsDir() {
			continue
		}
		name := e.Name()
		sockPath := filepath.Join(instancesDir, name, "status.sock")
		conn, err := net.DialTimeout("unix", sockPath, 500*time.Millisecond)
		if err != nil {
			continue
		}
		conn.Close()
		running = append(running, name)
	}
	return running
}

// isNoVM returns true if the error indicates no VM is running.
func isNoVM(err error) bool {
	s := err.Error()
	return strings.Contains(s, "no such file") ||
		strings.Contains(s, "connection refused")
}

// noVMError wraps the "no VM running" message.
func noVMError() error {
	return fmt.Errorf("no VM running")
}
