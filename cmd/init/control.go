//go:build linux

package main

import (
	"encoding/gob"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/http/pprof"
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

	mux := newGuestControlMux(gc)

	go http.Serve(ln, mux)

	vsockLn, err := vsock.Listen(protocol.GuestHTTPPort, nil)
	if err != nil {
		slog.Warn("guest control vsock listen failed", "error", err, "port", protocol.GuestHTTPPort)
		return
	}
	go http.Serve(vsockLn, mux)
}

func newGuestControlMux(gc *guestControl) *http.ServeMux {
	mux := http.NewServeMux()
	mux.HandleFunc("POST /checkpoint", gc.handleCheckpoint)
	mux.HandleFunc("POST /open", gc.handleOpen)
	mux.HandleFunc("POST /tcp/expose", gc.handleTCPExpose)
	mux.HandleFunc("GET /debug/pprof/", pprof.Index)
	mux.HandleFunc("GET /debug/pprof/cmdline", pprof.Cmdline)
	mux.HandleFunc("GET /debug/pprof/profile", pprof.Profile)
	mux.HandleFunc("GET /debug/pprof/symbol", pprof.Symbol)
	mux.HandleFunc("POST /debug/pprof/symbol", pprof.Symbol)
	mux.HandleFunc("GET /debug/pprof/trace", pprof.Trace)
	for _, name := range []string{
		"allocs",
		"block",
		"goroutine",
		"heap",
		"mutex",
		"threadcreate",
	} {
		mux.Handle("GET /debug/pprof/"+name, pprof.Handler(name))
	}
	return mux
}

type guestControl struct {
	mu  sync.Mutex
	enc *gob.Encoder
	dec *gob.Decoder
}

type guestTCPExpose struct {
	listenPort uint16
	host       string
	hostPort   uint16
	listener   net.Listener
}

var (
	guestTCPExposeMu sync.Mutex
	guestTCPExposes  = map[uint16]*guestTCPExpose{}
)

func guestInternalPort(port uint16) bool {
	guestTCPExposeMu.Lock()
	defer guestTCPExposeMu.Unlock()
	_, ok := guestTCPExposes[port]
	return ok
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

func (gc *guestControl) handleTCPExpose(w http.ResponseWriter, r *http.Request) {
	var req struct {
		ListenPort uint16 `json:"listen_port"`
		Host       string `json:"host"`
		HostPort   uint16 `json:"host_port"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "bad request", http.StatusBadRequest)
		return
	}
	if req.ListenPort == 0 || req.Host == "" || req.HostPort == 0 {
		http.Error(w, "listen_port, host, and host_port are required", http.StatusBadRequest)
		return
	}

	guestTCPExposeMu.Lock()
	if existing, ok := guestTCPExposes[req.ListenPort]; ok && existing != nil {
		if existing.host == req.Host && existing.hostPort == req.HostPort {
			guestTCPExposeMu.Unlock()
			w.Header().Set("Content-Type", "application/json")
			_ = json.NewEncoder(w).Encode(map[string]bool{"created": false})
			return
		}
		guestTCPExposeMu.Unlock()
		http.Error(w, fmt.Sprintf("port %d is already exposed", req.ListenPort), http.StatusConflict)
		return
	}
	guestTCPExposeMu.Unlock()

	ln, err := net.Listen("tcp", fmt.Sprintf(":%d", req.ListenPort))
	if err != nil {
		http.Error(w, fmt.Sprintf("listen on port %d: %v", req.ListenPort, err), http.StatusConflict)
		return
	}

	expose := &guestTCPExpose{
		listenPort: req.ListenPort,
		host:       req.Host,
		hostPort:   req.HostPort,
		listener:   ln,
	}

	guestTCPExposeMu.Lock()
	if existing, ok := guestTCPExposes[req.ListenPort]; ok && existing != nil {
		guestTCPExposeMu.Unlock()
		_ = ln.Close()
		if existing.host == req.Host && existing.hostPort == req.HostPort {
			w.Header().Set("Content-Type", "application/json")
			_ = json.NewEncoder(w).Encode(map[string]bool{"created": false})
			return
		}
		http.Error(w, fmt.Sprintf("port %d is already exposed", req.ListenPort), http.StatusConflict)
		return
	}
	guestTCPExposes[req.ListenPort] = expose
	guestTCPExposeMu.Unlock()

	go expose.acceptLoop()

	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(map[string]bool{"created": true})
}

func (e *guestTCPExpose) acceptLoop() {
	for {
		conn, err := e.listener.Accept()
		if err != nil {
			return
		}
		go e.forward(conn)
	}
}

func (e *guestTCPExpose) forward(src net.Conn) {
	defer src.Close()

	dst, err := net.Dial("tcp", net.JoinHostPort(e.host, itoa(int(e.hostPort))))
	if err != nil {
		return
	}
	defer dst.Close()

	done := make(chan struct{})
	go func() {
		io.Copy(dst, src)
		if tc, ok := dst.(*net.TCPConn); ok {
			_ = tc.CloseWrite()
		}
		close(done)
	}()
	io.Copy(src, dst)
	_ = src.Close()
	<-done
}
