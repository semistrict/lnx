package lnx

import (
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func TestHandleReadyIncludesGuestQueryError(t *testing.T) {
	s := newAPIServer(nil, "tester", "")

	req := httptest.NewRequest(http.MethodGet, "/ready", nil)
	rec := httptest.NewRecorder()
	s.handleReady(rec, req)

	if rec.Code != http.StatusServiceUnavailable {
		t.Fatalf("status = %d, want %d", rec.Code, http.StatusServiceUnavailable)
	}
	body := rec.Body.String()
	if !strings.Contains(body, "guest not ready: guest not connected") {
		t.Fatalf("body = %q, want guest query error", body)
	}
}
