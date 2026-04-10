//go:build linux

package main

import (
	"encoding/gob"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/semistrict/lnx/internal/protocol"
)

func TestGuestControlHandleLogSendsHostLogRequest(t *testing.T) {
	hostConn, guestConn := net.Pipe()
	defer hostConn.Close()
	defer guestConn.Close()

	done := make(chan struct{})
	go func() {
		defer close(done)
		dec := gob.NewDecoder(hostConn)
		enc := gob.NewEncoder(hostConn)

		var msg protocol.Msg
		if err := dec.Decode(&msg); err != nil {
			t.Errorf("decode log req: %v", err)
			return
		}
		if msg.LogReq == nil {
			t.Errorf("expected LogReq, got %#v", msg)
			return
		}
		if msg.LogReq.Level != "warn" {
			t.Errorf("level = %q, want warn", msg.LogReq.Level)
		}
		if msg.LogReq.Identifier != "lnx-xdg-open" {
			t.Errorf("identifier = %q, want lnx-xdg-open", msg.LogReq.Identifier)
		}
		if msg.LogReq.Message != "browser open failed" {
			t.Errorf("message = %q, want browser open failed", msg.LogReq.Message)
		}

		if err := enc.Encode(protocol.Msg{LogResp: &protocol.LogResp{}}); err != nil {
			t.Errorf("encode log resp: %v", err)
		}
	}()

	gc := &guestControl{
		enc: gob.NewEncoder(guestConn),
		dec: gob.NewDecoder(guestConn),
	}

	req := httptest.NewRequest(http.MethodPost, "/log?level=warn&identifier=lnx-xdg-open", strings.NewReader("browser open failed"))
	rec := httptest.NewRecorder()
	gc.handleLog(rec, req)

	if rec.Code != http.StatusNoContent {
		t.Fatalf("status = %d, body=%s", rec.Code, rec.Body.String())
	}
	<-done
}
