package lnx

import (
	"context"
	"io"
	"net"
	"net/http"
	"path/filepath"
	"strings"
	"testing"
)

func TestStatusSocketExposesPprof(t *testing.T) {
	dir := t.TempDir()
	sockPath := filepath.Join(dir, "status.sock")

	s := newAPIServer(nil, "tester", "")
	if err := s.listenUnix(sockPath); err != nil {
		t.Fatalf("listenUnix: %v", err)
	}
	t.Cleanup(func() {
		if s.listener != nil {
			_ = s.listener.Close()
		}
	})

	client := &http.Client{
		Transport: &http.Transport{
			DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
				var d net.Dialer
				return d.DialContext(ctx, "unix", sockPath)
			},
		},
	}

	for _, tc := range []struct {
		path string
		want string
	}{
		{path: "/debug/pprof/", want: "goroutine"},
		{path: "/debug/pprof/goroutine?debug=1", want: "goroutine profile"},
	} {
		resp, err := client.Get("http://unix" + tc.path)
		if err != nil {
			t.Fatalf("GET %s: %v", tc.path, err)
		}
		body, _ := io.ReadAll(resp.Body)
		_ = resp.Body.Close()
		if resp.StatusCode != http.StatusOK {
			t.Fatalf("GET %s status=%d body=%s", tc.path, resp.StatusCode, string(body))
		}
		if !strings.Contains(string(body), tc.want) {
			t.Fatalf("GET %s missing %q in body: %s", tc.path, tc.want, string(body))
		}
	}
}
