package main

import (
	"context"
	"fmt"
	"net"
	"net/http"
	"path/filepath"
	"strings"
	"time"
)

// apiClient returns an HTTP client that talks to the running VM's unix socket.
func apiClient() *http.Client {
	sockPath := filepath.Join(lnxDir(), "status.sock")
	return &http.Client{
		Transport: &http.Transport{
			DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
				return net.DialTimeout("unix", sockPath, 2*time.Second)
			},
		},
	}
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
