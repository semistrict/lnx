//go:build linux

package main

import (
	"encoding/gob"
	"encoding/json"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"os"
	"sync"

	"github.com/mdlayher/vsock"
	"github.com/semistrict/lnx/internal/protocol"
)

const guestControlSock = "/var/run/lnx/control.sock"

// startGuestControlServer dials the host on the guest control vsock port
// and starts an HTTP server on a unix socket inside the guest.
func startGuestControlServer() {
	hostConn, err := vsock.Dial(vsockHostCID, protocol.GuestControlPort, nil)
	if err != nil {
		slog.Warn("guest control vsock dial failed", "error", err)
		return
	}

	os.MkdirAll("/var/run/lnx", 0755)
	os.Remove(guestControlSock)

	ln, err := net.Listen("unix", guestControlSock)
	if err != nil {
		slog.Warn("guest control socket listen failed", "error", err)
		hostConn.Close()
		return
	}
	// Make it world-accessible so non-root users can curl it.
	os.Chmod(guestControlSock, 0666)

	gc := &guestControl{
		enc: gob.NewEncoder(hostConn),
		dec: gob.NewDecoder(hostConn),
	}

	mux := http.NewServeMux()
	mux.HandleFunc("POST /checkpoint", gc.handleCheckpoint)
	mux.HandleFunc("POST /open", gc.handleOpen)

	go http.Serve(ln, mux)
}

type guestControl struct {
	mu  sync.Mutex
	enc *gob.Encoder
	dec *gob.Decoder
}

func (gc *guestControl) handleCheckpoint(w http.ResponseWriter, r *http.Request) {
	gc.mu.Lock()
	defer gc.mu.Unlock()

	if err := gc.enc.Encode(protocol.Msg{CheckpointReq: &protocol.CheckpointReq{}}); err != nil {
		http.Error(w, fmt.Sprintf("send checkpoint request: %v", err), http.StatusInternalServerError)
		return
	}

	var msg protocol.Msg
	if err := gc.dec.Decode(&msg); err != nil {
		http.Error(w, fmt.Sprintf("read checkpoint response: %v", err), http.StatusInternalServerError)
		return
	}

	if msg.CheckpointResp == nil {
		http.Error(w, "unexpected response", http.StatusInternalServerError)
		return
	}

	w.Header().Set("Content-Type", "application/json")
	if msg.CheckpointResp.Error != "" {
		w.WriteHeader(http.StatusInternalServerError)
		json.NewEncoder(w).Encode(map[string]string{"error": msg.CheckpointResp.Error})
		return
	}

	json.NewEncoder(w).Encode(map[string]string{"path": msg.CheckpointResp.Path})
}

func (gc *guestControl) handleOpen(w http.ResponseWriter, r *http.Request) {
	var req struct {
		URL string `json:"url"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "bad request", http.StatusBadRequest)
		return
	}
	if req.URL == "" {
		http.Error(w, "url required", http.StatusBadRequest)
		return
	}

	gc.mu.Lock()
	defer gc.mu.Unlock()

	if err := gc.enc.Encode(protocol.Msg{OpenURLReq: &protocol.OpenURLReq{URL: req.URL}}); err != nil {
		http.Error(w, fmt.Sprintf("send open request: %v", err), http.StatusInternalServerError)
		return
	}

	var msg protocol.Msg
	if err := gc.dec.Decode(&msg); err != nil {
		http.Error(w, fmt.Sprintf("read open response: %v", err), http.StatusInternalServerError)
		return
	}

	if msg.OpenURLResp == nil {
		http.Error(w, "unexpected response", http.StatusInternalServerError)
		return
	}

	w.Header().Set("Content-Type", "application/json")
	if msg.OpenURLResp.Error != "" {
		w.WriteHeader(http.StatusInternalServerError)
		json.NewEncoder(w).Encode(map[string]string{"error": msg.OpenURLResp.Error})
		return
	}

	json.NewEncoder(w).Encode(map[string]string{"status": "ok"})
}
