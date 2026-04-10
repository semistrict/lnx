//go:build linux

package main

import (
	"context"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

func TestProxyInnerStatusFailsOnNonSuccessResponse(t *testing.T) {
	dir := t.TempDir()
	sockPath := filepath.Join(dir, "inner.sock")

	ln, err := net.Listen("unix", sockPath)
	if err != nil {
		t.Fatalf("listen unix: %v", err)
	}
	defer ln.Close()
	defer os.Remove(sockPath)

	server := &http.Server{
		Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			http.Error(w, "guest not ready", http.StatusServiceUnavailable)
		}),
	}
	defer server.Shutdown(context.Background())
	go server.Serve(ln)

	_, err = proxyInnerStatus(sockPath, false)
	if err == nil {
		t.Fatal("expected proxyInnerStatus error")
	}
}

func TestProxyInnerStatusFailsOnHungResponse(t *testing.T) {
	dir := t.TempDir()
	sockPath := filepath.Join(dir, "inner.sock")

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

	done := make(chan error, 1)
	go func() {
		_, err := proxyInnerStatus(sockPath, false)
		done <- err
	}()

	select {
	case err := <-done:
		if err == nil {
			t.Fatal("expected proxyInnerStatus error")
		}
	case <-time.After(2 * time.Second):
		t.Fatal("proxyInnerStatus hung")
	}
}

func TestWaitForInnerReadyOrExitWaitsForReady(t *testing.T) {
	dir := t.TempDir()
	sockPath := filepath.Join(dir, "inner.sock")

	ln, err := net.Listen("unix", sockPath)
	if err != nil {
		t.Fatalf("listen unix: %v", err)
	}
	defer ln.Close()
	defer os.Remove(sockPath)

	var ready atomic.Bool
	server := &http.Server{
		Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			if r.URL.Path != "/ready" {
				http.NotFound(w, r)
				return
			}
			if !ready.Load() {
				http.Error(w, "not ready", http.StatusServiceUnavailable)
				return
			}
			w.WriteHeader(http.StatusNoContent)
		}),
	}
	defer server.Shutdown(context.Background())
	go server.Serve(ln)

	innerErrCh := make(chan error, 1)
	go func() {
		time.Sleep(300 * time.Millisecond)
		ready.Store(true)
	}()

	start := time.Now()
	if err := waitForInnerReadyOrExit(sockPath, innerErrCh, 2*time.Second); err != nil {
		t.Fatalf("waitForInnerReadyOrExit: %v", err)
	}
	if elapsed := time.Since(start); elapsed < 300*time.Millisecond {
		t.Fatalf("returned before inner daemon was ready: %v", elapsed)
	}
}

func TestProbeInnerReadyFallsBackToExec(t *testing.T) {
	dir := t.TempDir()
	sockPath := filepath.Join(dir, "inner.sock")

	ln, err := net.Listen("unix", sockPath)
	if err != nil {
		t.Fatalf("listen unix: %v", err)
	}
	defer ln.Close()
	defer os.Remove(sockPath)

	server := &http.Server{
		Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			switch r.URL.Path {
			case "/ready":
				http.Error(w, "status not ready", http.StatusServiceUnavailable)
			case "/exec":
				w.Header().Set("Content-Type", "application/json")
				_, _ = w.Write([]byte("{\"exit_code\":0}\n"))
			default:
				http.NotFound(w, r)
			}
		}),
	}
	defer server.Shutdown(context.Background())
	go server.Serve(ln)

	stage, err := probeInnerReadyDetailed(sockPath)
	if err != nil {
		t.Fatalf("probeInnerReady: %v", err)
	}
	if stage != "exec_probe" {
		t.Fatalf("stage = %q, want exec_probe", stage)
	}
}

func TestProbeInnerReadyDetailedIncludesReadyAndExecFailures(t *testing.T) {
	dir := t.TempDir()
	sockPath := filepath.Join(dir, "inner.sock")

	ln, err := net.Listen("unix", sockPath)
	if err != nil {
		t.Fatalf("listen unix: %v", err)
	}
	defer ln.Close()
	defer os.Remove(sockPath)

	server := &http.Server{
		Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			switch r.URL.Path {
			case "/ready":
				http.Error(w, "status not ready", http.StatusServiceUnavailable)
			case "/exec":
				http.Error(w, "exec not ready", http.StatusServiceUnavailable)
			default:
				http.NotFound(w, r)
			}
		}),
	}
	defer server.Shutdown(context.Background())
	go server.Serve(ln)

	stage, err := probeInnerReadyDetailed(sockPath)
	if err == nil {
		t.Fatal("expected probeInnerReadyDetailed error")
	}
	if stage != "exec_probe" {
		t.Fatalf("stage = %q, want exec_probe", stage)
	}
	if !strings.Contains(err.Error(), "inner /ready returned 503") {
		t.Fatalf("error %q missing /ready failure", err)
	}
	if !strings.Contains(err.Error(), "inner exec probe failed") {
		t.Fatalf("error %q missing exec probe failure", err)
	}
}

func TestInnerStatusSockPathUsesLinuxHostRuntimeDir(t *testing.T) {
	got := innerStatusSockPath("/var/lib/lnx/memorysnapshot/inner")
	want := "/var/run/lnx/inner/status.sock"
	if got != want {
		t.Fatalf("innerStatusSockPath() = %q, want %q", got, want)
	}
}

func TestFilterInnerEnvDoesNotProvideExperiments(t *testing.T) {
	got := filterInnerEnv([]string{
		"FOO=bar",
		"LNX_TOPLEVEL_MODE=memorysnapshot",
		"LNX_TOPLEVEL_INSTANCE=default",
		"LNX_BASE=/tmp/base",
	})
	if strings.Join(got, ",") != "FOO=bar" {
		t.Fatalf("filterInnerEnv() = %v, want only non-top-level env", got)
	}
}

func TestGuestControlProxyMuxProxiesOpenAndExposeRoutes(t *testing.T) {
	dir := t.TempDir()
	sockPath := filepath.Join(dir, "inner.sock")

	ln, err := net.Listen("unix", sockPath)
	if err != nil {
		t.Fatalf("listen unix: %v", err)
	}
	defer ln.Close()
	defer os.Remove(sockPath)

	server := &http.Server{
		Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			switch {
			case r.Method == http.MethodPost && r.URL.Path == "/open":
				w.Header().Set("Content-Type", "application/json")
				_, _ = io.WriteString(w, `{"status":"ok"}`)
			case r.Method == http.MethodPost && r.URL.Path == "/tcp/expose":
				w.Header().Set("Content-Type", "application/json")
				_, _ = io.WriteString(w, `{"created":true}`)
			default:
				http.NotFound(w, r)
			}
		}),
	}
	defer server.Shutdown(context.Background())
	go server.Serve(ln)

	mux := newGuestControlProxyMux(sockPath)

	openReq := httptest.NewRequest(http.MethodPost, "/open", io.NopCloser(strings.NewReader(`{"url":"https://example.com"}`)))
	openReq.Header.Set("Content-Type", "application/json")
	openRec := httptest.NewRecorder()
	mux.ServeHTTP(openRec, openReq)
	if openRec.Code != http.StatusOK {
		t.Fatalf("open status = %d, body=%s", openRec.Code, openRec.Body.String())
	}

	exposeReq := httptest.NewRequest(http.MethodPost, "/tcp/expose", io.NopCloser(strings.NewReader(`{"listen_port":1234,"host":"host","host_port":4321}`)))
	exposeReq.Header.Set("Content-Type", "application/json")
	exposeRec := httptest.NewRecorder()
	mux.ServeHTTP(exposeRec, exposeReq)
	if exposeRec.Code != http.StatusOK {
		t.Fatalf("tcp/expose status = %d, body=%s", exposeRec.Code, exposeRec.Body.String())
	}
}
