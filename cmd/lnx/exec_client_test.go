package main

import (
	"context"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestVMReadyAtSocketTimesOut(t *testing.T) {
	dir := t.TempDir()
	sockPath := filepath.Join(dir, "status.sock")

	ln, err := net.Listen("unix", sockPath)
	if err != nil {
		t.Fatalf("listen unix: %v", err)
	}
	defer ln.Close()
	defer os.Remove(sockPath)

	server := &http.Server{
		Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			<-r.Context().Done()
		}),
	}
	defer server.Shutdown(context.Background())
	go server.Serve(ln)

	start := time.Now()
	ready, err := vmReadyAtSocket(sockPath)
	if err == nil {
		t.Fatal("expected timeout error")
	}
	if ready {
		t.Fatal("expected not ready")
	}
	if elapsed := time.Since(start); elapsed > 2*time.Second {
		t.Fatalf("vmReadyAtSocket took too long: %v", elapsed)
	}
}

func TestRunVMReturnsMemorySnapshotBootErrorOnStartupTimeout(t *testing.T) {
	oldInstance := instanceName
	oldCheckpoint := doCheckpoint
	oldEphemeral := doEphemeral
	oldSSHAgent := doSSHAgent
	oldForwardEnv := append([]string(nil), forwardEnv...)
	oldForwardAllEnv := forwardAllEnv
	instanceName = "test-ms-booterr"
	doCheckpoint = false
	doEphemeral = false
	doSSHAgent = false
	forwardEnv = nil
	forwardAllEnv = false
	t.Cleanup(func() {
		instanceName = oldInstance
		doCheckpoint = oldCheckpoint
		doEphemeral = oldEphemeral
		doSSHAgent = oldSSHAgent
		forwardEnv = oldForwardEnv
		forwardAllEnv = oldForwardAllEnv
	})

	base := t.TempDir()
	t.Setenv("HOME", base)
	t.Setenv("LNX_EXPERIMENTS", "memorysnapshot")

	instDir := filepath.Join(base, ".lnx", "instances", instanceName)
	if err := os.MkdirAll(instDir, 0755); err != nil {
		t.Fatalf("mkdir instance dir: %v", err)
	}
	if err := os.WriteFile(filepath.Join(base, ".lnx", "vmlinuz"), []byte("kernel"), 0644); err != nil {
		t.Fatalf("write kernel: %v", err)
	}
	if err := os.WriteFile(filepath.Join(instDir, "rootfs.ext4"), []byte("rootfs"), 0644); err != nil {
		t.Fatalf("write rootfs: %v", err)
	}
	if err := os.MkdirAll(filepath.Join(instDir, "memorysnapshot"), 0755); err != nil {
		t.Fatalf("mkdir memorysnapshot dir: %v", err)
	}
	want := "restore failed for test"
	if err := os.WriteFile(filepath.Join(instDir, "memorysnapshot", "boot-error.log"), []byte(want+"\n"), 0644); err != nil {
		t.Fatalf("write boot-error.log: %v", err)
	}

	oldSpawnDaemon := spawnDaemonFn
	oldWaitForVM := waitForVMFn
	oldVMIsRunning := vmIsRunningFn
	spawnDaemonFn = func() error { return nil }
	waitForVMFn = func(timeout time.Duration) error { return context.DeadlineExceeded }
	vmIsRunningFn = func() bool { return false }
	t.Cleanup(func() {
		spawnDaemonFn = oldSpawnDaemon
		waitForVMFn = oldWaitForVM
		vmIsRunningFn = oldVMIsRunning
	})

	_, err := runVM([]string{"true"})
	if err == nil || err.Error() != want {
		t.Fatalf("runVM error = %v, want %q", err, want)
	}
}
