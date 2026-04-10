package lnx

import (
	"encoding/gob"
	"net"
	"testing"

	"github.com/semistrict/lnx/internal/protocol"
)

func TestOpenURLOnHostRejectsNonHTTPURLs(t *testing.T) {
	called := false
	old := launchHostURLOpener
	launchHostURLOpener = func(string) error {
		called = true
		return nil
	}
	defer func() { launchHostURLOpener = old }()

	err := openURLOnHost("file:///tmp/nope")
	if err == nil {
		t.Fatal("expected openURLOnHost to reject non-http URL")
	}
	if called {
		t.Fatal("launcher should not be called for rejected URL")
	}
}

func TestHandleGuestCtrlOpenURLUsesHostLauncher(t *testing.T) {
	hostConn, guestConn := net.Pipe()
	defer hostConn.Close()
	defer guestConn.Close()

	old := launchHostURLOpener
	var gotURL string
	launchHostURLOpener = func(url string) error {
		gotURL = url
		return nil
	}
	defer func() { launchHostURLOpener = old }()

	s := newAPIServer(nil, "tester", "")
	done := make(chan struct{})
	go func() {
		s.handleGuestCtrl(hostConn)
		close(done)
	}()

	enc := gob.NewEncoder(guestConn)
	dec := gob.NewDecoder(guestConn)
	if err := enc.Encode(protocol.Msg{OpenURLReq: &protocol.OpenURLReq{URL: "https://example.com"}}); err != nil {
		t.Fatalf("encode open request: %v", err)
	}

	var msg protocol.Msg
	if err := dec.Decode(&msg); err != nil {
		t.Fatalf("decode open response: %v", err)
	}
	if msg.OpenURLResp == nil {
		t.Fatal("expected OpenURLResp")
	}
	if msg.OpenURLResp.Error != "" {
		t.Fatalf("unexpected open response error: %s", msg.OpenURLResp.Error)
	}
	if gotURL != "https://example.com" {
		t.Fatalf("launcher URL = %q, want %q", gotURL, "https://example.com")
	}

	_ = guestConn.Close()
	<-done
}

func TestHandleGuestCtrlLogReqResponds(t *testing.T) {
	hostConn, guestConn := net.Pipe()
	defer hostConn.Close()
	defer guestConn.Close()

	s := newAPIServer(nil, "tester", "")
	done := make(chan struct{})
	go func() {
		s.handleGuestCtrl(hostConn)
		close(done)
	}()

	enc := gob.NewEncoder(guestConn)
	dec := gob.NewDecoder(guestConn)
	if err := enc.Encode(protocol.Msg{LogReq: &protocol.LogReq{
		Level:      "info",
		Identifier: "lnx-xdg-open",
		Message:    "forwarded browser open url=https://example.com",
	}}); err != nil {
		t.Fatalf("encode log request: %v", err)
	}

	var msg protocol.Msg
	if err := dec.Decode(&msg); err != nil {
		t.Fatalf("decode log response: %v", err)
	}
	if msg.LogResp == nil {
		t.Fatal("expected LogResp")
	}
	if msg.LogResp.Error != "" {
		t.Fatalf("unexpected log response error: %s", msg.LogResp.Error)
	}

	_ = guestConn.Close()
	<-done
}
