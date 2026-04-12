package lnx

import (
	"bytes"
	"context"
	"encoding/gob"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/http/pprof"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"nhooyr.io/websocket"

	"github.com/semistrict/lnx/internal/protocol"
)

// idleTimeout is how long the daemon waits after the last exec session
// finishes before shutting down. This allows back-to-back commands like
// `lnx echo hello && lnx echo world` to reuse the same VM.
const idleTimeout = 5 * time.Second

// StatusResponse is the JSON structure served to `lnx status` clients.
type StatusResponse struct {
	Command     []string `json:"command"`
	User        string   `json:"user"`
	UptimeSecs  float64  `json:"uptime_secs"`
	MemTotalKB  uint64   `json:"mem_total_kb"`
	MemAvailKB  uint64   `json:"mem_avail_kb"`
	SwapTotalKB uint64   `json:"swap_total_kb"`
	SwapFreeKB  uint64   `json:"swap_free_kb"`
	DiskTotalKB uint64   `json:"disk_total_kb"`
	DiskUsedKB  uint64   `json:"disk_used_kb"`
	LoadAvg     string   `json:"load_avg"`
	Dmesg       string   `json:"dmesg,omitempty"`
}

// apiServer manages the guest vsock connections and the host unix
// socket HTTP server for status queries and exec requests.
type apiServer struct {
	args         []string
	user         string
	startTime    time.Time
	rootfsPath   string
	criuPath     string
	instanceName string
	instanceDir  string
	statusMu   sync.Mutex
	statusEnc  *gob.Encoder
	statusDec  *gob.Decoder
	statusConn net.Conn

	guestCtrlConn net.Conn

	sock VsockDevice
	pf   *portForwarder

	sockPath string
	listener net.Listener

	// Session tracking for daemon mode.
	activeExecs atomic.Int64
	pinRefs     atomic.Int64
	idleMu      sync.Mutex    // protects idleTimer and idleCh close
	idleTimer   *time.Timer   // runs after last exec finishes; fires to close idleCh
	idleCh      chan struct{} // closed when idle timeout expires
	stopCh      chan struct{} // closed when /stop is called
	stopOnce    sync.Once

	sessionsMu sync.RWMutex
	sessions   map[string]*SessionInfo
	sessionSeq atomic.Int64
}

// SessionInfo describes an active exec session.
type SessionInfo struct {
	ID        string    `json:"id"`
	Args      []string  `json:"args"`
	PTY       bool      `json:"pty"`
	StartTime time.Time `json:"start_time"`
	ClientPID int       `json:"client_pid,omitempty"`
	GuestPID  int       `json:"guest_pid,omitempty"`

	// Internal: gob encoder to send signals to the guest process.
	// encMu serializes writes; gob.Encoder is not goroutine-safe.
	encMu   sync.Mutex
	execEnc *gob.Encoder

	// Internal: connections to close when killing the session.
	execConn  net.Conn // gob exec connection (port 1027)
	vsockConn net.Conn // raw PTY vsock connection (port 1032), nil for non-PTY
}

// encodeExec sends a gob message on the session's exec connection, safely.
func (s *SessionInfo) encodeExec(msg protocol.Msg) error {
	s.encMu.Lock()
	defer s.encMu.Unlock()
	return s.execEnc.Encode(msg)
}

func newAPIServer(args []string, user, rootfsPath string) *apiServer {
	return &apiServer{
		args:       args,
		user:       user,
		rootfsPath: rootfsPath,
		startTime:  time.Now(),
		idleCh:     make(chan struct{}),
		stopCh:     make(chan struct{}),
		sessions:   make(map[string]*SessionInfo),
	}
}

func (s *apiServer) requestStop(reason string, attrs ...any) {
	if reason != "" {
		slog.Warn(reason, attrs...)
	}
	s.stopOnce.Do(func() {
		close(s.stopCh)
	})
}

// registerSession creates a new session entry and returns its ID.
func (s *apiServer) registerSession(args []string, pty bool, clientPID int, execEnc *gob.Encoder, execConn net.Conn) string {
	id := fmt.Sprintf("s%d", s.sessionSeq.Add(1))
	s.sessionsMu.Lock()
	s.sessions[id] = &SessionInfo{
		ID:        id,
		Args:      args,
		PTY:       pty,
		StartTime: time.Now(),
		ClientPID: clientPID,
		execEnc:   execEnc,
		execConn:  execConn,
	}
	s.sessionsMu.Unlock()
	return id
}

// setSessionVsockConn stores the PTY vsock connection for a session.
func (s *apiServer) setSessionVsockConn(id string, conn net.Conn) {
	s.sessionsMu.Lock()
	if sess, ok := s.sessions[id]; ok {
		sess.vsockConn = conn
	}
	s.sessionsMu.Unlock()
}

// setSessionGuestPID records the guest PID for a session.
func (s *apiServer) setSessionGuestPID(id string, pid int) {
	s.sessionsMu.Lock()
	if sess, ok := s.sessions[id]; ok {
		sess.GuestPID = pid
	}
	s.sessionsMu.Unlock()
}

// closeSession forcefully closes a session's connections, causing the
// WebSocket handler and guest exec to clean up. Returns false if not found.
func (s *apiServer) closeSession(id string) bool {
	s.sessionsMu.RLock()
	sess, ok := s.sessions[id]
	s.sessionsMu.RUnlock()
	if !ok {
		return false
	}
	if sess.vsockConn != nil {
		sess.vsockConn.Close()
	}
	if sess.execConn != nil {
		sess.execConn.Close()
	}
	return true
}

// signalSession sends a signal to a session's guest process. Returns false if not found.
func (s *apiServer) signalSession(id string, sig int) bool {
	s.sessionsMu.RLock()
	sess, ok := s.sessions[id]
	s.sessionsMu.RUnlock()
	if !ok || sess.execEnc == nil {
		return false
	}
	sess.encodeExec(protocol.Msg{ExecSignal: &protocol.ExecSignal{Sig: sig}})
	return true
}

// unregisterSession removes a session and decrements the exec counter.
// If the counter drops to zero, the idle timer is started.
func (s *apiServer) unregisterSession(id string) {
	s.sessionsMu.Lock()
	delete(s.sessions, id)
	s.sessionsMu.Unlock()
}

// execStarted increments the active exec counter (for non-tracked sessions like Run()).
// If the counter was zero (idle timer may be running), the timer is cancelled.
func (s *apiServer) execStarted() {
	if s.activeExecs.Add(1) == 1 {
		s.cancelIdleTimer()
	}
}

// execFinished decrements the active exec counter. If it drops to zero,
// the idle timer is started.
func (s *apiServer) execFinished() {
	if s.activeExecs.Add(-1) == 0 {
		s.startIdleTimer()
	}
}

// startIdleTimer begins the idle countdown. If the timer fires without
// being cancelled by a new exec session, idleCh is closed and the daemon
// will shut down.
func (s *apiServer) startIdleTimer() {
	s.idleMu.Lock()
	defer s.idleMu.Unlock()

	select {
	case <-s.idleCh:
		return // already shut down
	default:
	}

	s.idleTimer = time.AfterFunc(idleTimeout, func() {
		s.idleMu.Lock()
		defer s.idleMu.Unlock()
		if s.activeExecs.Load() == 0 && s.pinRefs.Load() == 0 {
			select {
			case <-s.idleCh:
			default:
				slog.Info("idle timeout expired, shutting down")
				close(s.idleCh)
			}
		}
	})
}

// cancelIdleTimer stops a pending idle timer if one is running.
func (s *apiServer) cancelIdleTimer() {
	s.idleMu.Lock()
	defer s.idleMu.Unlock()
	if s.idleTimer != nil {
		s.idleTimer.Stop()
		s.idleTimer = nil
	}
}

// WaitIdle blocks until there are no active exec sessions, or stop is requested.
func (s *apiServer) WaitIdle() {
	select {
	case <-s.idleCh:
	case <-s.stopCh:
	}
}

func (s *apiServer) pin() {
	if s.pinRefs.Add(1) == 1 {
		s.cancelIdleTimer()
	}
}

func (s *apiServer) unpin() {
	n := s.pinRefs.Add(-1)
	if n < 0 {
		s.pinRefs.Store(0)
		n = 0
	}
	if n == 0 && s.activeExecs.Load() == 0 {
		s.startIdleTimer()
	}
}

// setStatusConn stores the guest's status vsock connection.
func (s *apiServer) setStatusConn(conn net.Conn) {
	s.statusMu.Lock()
	defer s.statusMu.Unlock()
	s.statusConn = conn
	s.statusEnc = gob.NewEncoder(conn)
	s.statusDec = gob.NewDecoder(conn)
}

// connectExec creates a new vsock connection to the guest's exec server.
// Retries briefly since the guest may still be booting.
func (s *apiServer) connectExec() (*gob.Encoder, *gob.Decoder, net.Conn, error) {
	var conn net.Conn
	var err error
	for i := 0; i < 300; i++ {
		conn, err = s.sock.Connect(protocol.ExecPort)
		if err == nil {
			return gob.NewEncoder(conn), gob.NewDecoder(conn), conn, nil
		}
		if errSuggestsDeadVM(err) {
			s.requestStop("vm no longer live during exec connect", "error", err)
			return nil, nil, nil, err
		}
		time.Sleep(200 * time.Millisecond)
	}
	return nil, nil, nil, err
}

// setGuestCtrlConn stores the guest control vsock connection and starts
// handling guest-initiated requests (checkpoint, etc).
func (s *apiServer) setGuestCtrlConn(conn net.Conn) {
	s.guestCtrlConn = conn
	go s.handleGuestCtrl(conn)
}

func (s *apiServer) handleGuestCtrl(conn net.Conn) {
	defer conn.Close()
	enc := gob.NewEncoder(conn)
	dec := gob.NewDecoder(conn)

	for {
		var msg protocol.Msg
		if err := dec.Decode(&msg); err != nil {
			return
		}

		if msg.CheckpointReq != nil {
			cpDir := filepath.Join(filepath.Dir(s.rootfsPath), "checkpoints")
			cpPath, err := CreateCheckpoint(s.rootfsPath, cpDir, msg.CheckpointReq.Name)
			resp := &protocol.CheckpointResp{}
			if err != nil {
				resp.Error = err.Error()
			} else {
				resp.Path = cpPath
			}
			if err := enc.Encode(protocol.Msg{CheckpointResp: resp}); err != nil {
				return
			}
		}

		if msg.OpenURLReq != nil {
			resp := &protocol.OpenURLResp{}
			u := msg.OpenURLReq.URL
			if !strings.HasPrefix(u, "http://") && !strings.HasPrefix(u, "https://") {
				resp.Error = "only http:// and https:// URLs are allowed"
			} else if err := exec.Command("open", u).Run(); err != nil {
				resp.Error = err.Error()
			}
			if err := enc.Encode(protocol.Msg{OpenURLResp: resp}); err != nil {
				return
			}
		}

		if msg.ForkReq != nil {
			resp := s.executeFork()
			if err := enc.Encode(protocol.Msg{ForkResp: resp}); err != nil {
				return
			}
		}
	}
}

// executeFork handles the fork orchestration (shared between HTTP and gob paths).
func (s *apiServer) executeFork() *protocol.ForkResp {
	if s.instanceName == "" || s.instanceDir == "" {
		return &protocol.ForkResp{Error: "fork requires daemon mode"}
	}

	// Generate child name with millisecond precision to avoid collisions.
	childName := s.instanceName + "-fork-" + time.Now().Format("150405.000")
	childName = strings.ReplaceAll(childName, ".", "")
	instancesDir := filepath.Dir(s.instanceDir)
	childDir := filepath.Join(instancesDir, childName)
	if err := os.MkdirAll(childDir, 0755); err != nil {
		return &protocol.ForkResp{Error: fmt.Sprintf("create child dir: %v", err)}
	}

	// APFS clone rootfs.
	childRootfs := filepath.Join(childDir, "rootfs.ext4")
	if err := cloneFile(s.rootfsPath, childRootfs); err != nil {
		os.RemoveAll(childDir)
		return &protocol.ForkResp{Error: fmt.Sprintf("clone rootfs: %v", err)}
	}

	// APFS clone CRIU volume (contains fork dump images).
	if s.criuPath != "" {
		childCRIU := filepath.Join(childDir, "criu.ext4")
		if err := cloneFile(s.criuPath, childCRIU); err != nil {
			os.RemoveAll(childDir)
			return &protocol.ForkResp{Error: fmt.Sprintf("clone criu volume: %v", err)}
		}
	}

	// Copy instance metadata.
	copyForkMetadata(s.instanceDir, childDir)

	// Spawn child daemon.
	self, err := os.Executable()
	if err != nil {
		os.RemoveAll(childDir)
		return &protocol.ForkResp{Error: fmt.Sprintf("get executable: %v", err)}
	}

	childCmd := exec.Command(self, "_daemon", "--instance", childName)
	childCmd.Env = os.Environ()
	if err := childCmd.Start(); err != nil {
		os.RemoveAll(childDir)
		return &protocol.ForkResp{Error: fmt.Sprintf("start child daemon: %v", err)}
	}
	childCmd.Process.Release()

	// Wait for child to be ready.
	childSock := filepath.Join(childDir, "status.sock")
	for i := 0; i < 300; i++ {
		if conn, err := net.DialTimeout("unix", childSock, 200*time.Millisecond); err == nil {
			conn.Close()
			break
		}
		time.Sleep(200 * time.Millisecond)
	}

	return &protocol.ForkResp{Instance: childName}
}

// queryGuest sends a StatusReq and reads the StatusResp.
func (s *apiServer) queryGuest(includeDmesg bool) (*protocol.StatusResp, error) {
	s.statusMu.Lock()
	defer s.statusMu.Unlock()

	if s.statusEnc == nil {
		return nil, fmt.Errorf("guest not connected")
	}

	if err := s.statusEnc.Encode(protocol.Msg{StatusReq: &protocol.StatusReq{
		IncludeDmesg: includeDmesg,
	}}); err != nil {
		return nil, fmt.Errorf("encode status req: %w", err)
	}

	var msg protocol.Msg
	if err := s.statusDec.Decode(&msg); err != nil {
		return nil, fmt.Errorf("decode status resp: %w", err)
	}
	if msg.StatusResp == nil {
		return nil, fmt.Errorf("expected StatusResp, got %+v", msg)
	}
	return msg.StatusResp, nil
}

// listenUnix starts the HTTP server on a unix socket.
func (s *apiServer) listenUnix(sockPath string) error {
	os.Remove(sockPath)
	ln, err := net.Listen("unix", sockPath)
	if err != nil {
		return fmt.Errorf("listen unix %s: %w", sockPath, err)
	}
	// Make the socket world-accessible so non-root clients can connect
	// (the daemon may run as root while clients run as the regular user).
	os.Chmod(sockPath, 0666)
	s.sockPath = sockPath
	s.listener = ln

	mux := http.NewServeMux()
	mux.HandleFunc("GET /status", s.handleStatus)
	mux.HandleFunc("GET /ports", s.handlePorts)
	mux.HandleFunc("POST /checkpoint", s.handleCheckpoint)
	mux.HandleFunc("POST /criu/checkpoint", s.handleCRIUCheckpoint)
	mux.HandleFunc("POST /fork", s.handleFork)
	mux.HandleFunc("POST /expose/host", s.handleExposeHost)
	mux.HandleFunc("POST /expose/host/remove", s.handleRemoveExposeHost)
	mux.HandleFunc("POST /guest/expose", s.handleGuestExpose)
	mux.HandleFunc("POST /exec", s.handleExec)
	mux.HandleFunc("GET /exec/ws", s.handleExecWS)
	mux.HandleFunc("GET /fork/ws", s.handleForkAttachWS)
	mux.HandleFunc("GET /sessions", s.handleSessions)
	mux.HandleFunc("POST /sessions/kill", s.handleSessionKill)
	mux.HandleFunc("POST /stop", s.handleStop)
	mux.HandleFunc("/guest/debug/pprof/", s.handleGuestPprofProxy)
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

	go http.Serve(ln, mux)
	return nil
}

func (s *apiServer) handleGuestPprofProxy(w http.ResponseWriter, r *http.Request) {
	if s.sock == nil {
		http.Error(w, "guest vsock unavailable", http.StatusServiceUnavailable)
		return
	}
	targetURL := "http://guest" + strings.TrimPrefix(r.URL.RequestURI(), "/guest")
	req, err := http.NewRequestWithContext(r.Context(), r.Method, targetURL, r.Body)
	if err != nil {
		http.Error(w, fmt.Sprintf("build proxy request: %v", err), http.StatusInternalServerError)
		return
	}
	req.Header = r.Header.Clone()

	client := &http.Client{
		Transport: &http.Transport{
			DialContext: func(_ context.Context, _, _ string) (net.Conn, error) {
				return s.sock.Connect(protocol.GuestHTTPPort)
			},
		},
	}

	resp, err := client.Do(req)
	if err != nil {
		http.Error(w, fmt.Sprintf("guest pprof proxy: %v", err), http.StatusBadGateway)
		return
	}
	defer resp.Body.Close()

	for k, vals := range resp.Header {
		for _, v := range vals {
			w.Header().Add(k, v)
		}
	}
	w.WriteHeader(resp.StatusCode)
	_, _ = io.Copy(w, resp.Body)
}

func (s *apiServer) handleStatus(w http.ResponseWriter, r *http.Request) {
	includeDmesg := r.URL.Query().Get("dmesg") == "1"

	resp := StatusResponse{
		Command: s.args,
		User:    s.user,
	}

	guestResp, err := s.queryGuest(includeDmesg)
	if err != nil {
		slog.Debug("status guest query failed", "error", err)
		resp.UptimeSecs = time.Since(s.startTime).Seconds()
	} else {
		resp.UptimeSecs = guestResp.UptimeSecs
		resp.MemTotalKB = guestResp.MemTotalKB
		resp.MemAvailKB = guestResp.MemAvailKB
		resp.SwapTotalKB = guestResp.SwapTotalKB
		resp.SwapFreeKB = guestResp.SwapFreeKB
		resp.DiskTotalKB = guestResp.DiskTotalKB
		resp.DiskUsedKB = guestResp.DiskUsedKB
		resp.LoadAvg = guestResp.LoadAvg
		resp.Dmesg = guestResp.Dmesg
	}

	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(resp)
}

// PortEntry describes a single forwarded port.
type PortEntry struct {
	Guest uint16 `json:"guest"`
	Host  uint16 `json:"host"`
}

type ExposeHostRequest struct {
	GuestPort uint16 `json:"guest_port"`
	HostPort  uint16 `json:"host_port,omitempty"`
	Visible   bool   `json:"visible,omitempty"`
}

type ExposeHostResponse struct {
	HostPort uint16 `json:"host_port"`
	Created  bool   `json:"created"`
}

type RemoveExposeHostRequest struct {
	HostPort uint16 `json:"host_port"`
}

type GuestExposeRequest struct {
	ListenPort uint16 `json:"listen_port"`
	Host       string `json:"host"`
	HostPort   uint16 `json:"host_port"`
}

type GuestExposeResponse struct {
	Created bool `json:"created"`
}

func (s *apiServer) handlePorts(w http.ResponseWriter, r *http.Request) {
	var ports []PortEntry
	if s.pf != nil {
		ports = s.pf.listVisiblePorts()
	}
	if ports == nil {
		ports = []PortEntry{}
	}
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(ports)
}

func (s *apiServer) handleExposeHost(w http.ResponseWriter, r *http.Request) {
	if s.pf == nil {
		http.Error(w, "port forwarding unavailable", http.StatusServiceUnavailable)
		return
	}

	var req ExposeHostRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "bad request", http.StatusBadRequest)
		return
	}
	if req.GuestPort == 0 {
		http.Error(w, "guest_port required", http.StatusBadRequest)
		return
	}

	hostPort, created, err := s.pf.exposeHost(req.GuestPort, req.HostPort, req.Visible)
	if err != nil {
		http.Error(w, err.Error(), http.StatusConflict)
		return
	}
	if created {
		s.pin()
	}

	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(ExposeHostResponse{HostPort: hostPort, Created: created})
}

func (s *apiServer) handleRemoveExposeHost(w http.ResponseWriter, r *http.Request) {
	if s.pf == nil {
		http.Error(w, "port forwarding unavailable", http.StatusServiceUnavailable)
		return
	}

	var req RemoveExposeHostRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "bad request", http.StatusBadRequest)
		return
	}
	if req.HostPort == 0 {
		http.Error(w, "host_port required", http.StatusBadRequest)
		return
	}
	if !s.pf.removeHost(req.HostPort) {
		http.Error(w, fmt.Sprintf("no manual host forward for port %d", req.HostPort), http.StatusNotFound)
		return
	}
	s.unpin()
	w.WriteHeader(http.StatusNoContent)
}

func (s *apiServer) handleGuestExpose(w http.ResponseWriter, r *http.Request) {
	if s.sock == nil {
		http.Error(w, "guest vsock unavailable", http.StatusServiceUnavailable)
		return
	}

	var req GuestExposeRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "bad request", http.StatusBadRequest)
		return
	}
	if req.ListenPort == 0 || req.HostPort == 0 || req.Host == "" {
		http.Error(w, "listen_port, host, and host_port are required", http.StatusBadRequest)
		return
	}

	var resp GuestExposeResponse
	if err := s.proxyGuestJSON(r.Context(), "/tcp/expose", req, &resp); err != nil {
		http.Error(w, err.Error(), http.StatusBadGateway)
		return
	}
	if resp.Created {
		s.pin()
	}
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(resp)
}

// handleCRIUCheckpoint orchestrates a CRIU checkpoint:
//  1. Guest dumps processes to the CRIU block device (/mnt/criu/<name>/)
//  2. Host APFS-clones both rootfs.ext4 and criu.ext4
//  3. Guest cleans up dump images from the live CRIU volume
//
// The checkpoint directory contains rootfs.ext4 + criu.ext4, both instant
// copy-on-write clones.
func (s *apiServer) handleCRIUCheckpoint(w http.ResponseWriter, r *http.Request) {
	if s.sock == nil {
		http.Error(w, "guest vsock unavailable", http.StatusServiceUnavailable)
		return
	}
	if s.criuPath == "" {
		http.Error(w, "CRIU volume not configured", http.StatusBadRequest)
		return
	}

	var req struct {
		Name string `json:"name"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "bad request", http.StatusBadRequest)
		return
	}
	if req.Name == "" {
		http.Error(w, "name required", http.StatusBadRequest)
		return
	}

	// Step 1: Guest dumps processes to CRIU volume.
	var dumpResp struct {
		Status string `json:"status"`
	}
	if err := s.proxyGuestJSON(r.Context(), "/criu/dump", req, &dumpResp); err != nil {
		http.Error(w, fmt.Sprintf("guest CRIU dump: %v", err), http.StatusBadGateway)
		return
	}

	// Step 2: APFS-clone rootfs + CRIU volume into checkpoint dir.
	cpDir := filepath.Join(filepath.Dir(s.rootfsPath), "checkpoints", req.Name)
	cpPath, err := CreateCRIUCheckpoint(s.rootfsPath, s.criuPath, cpDir)
	if err != nil {
		http.Error(w, fmt.Sprintf("clone: %v", err), http.StatusInternalServerError)
		return
	}

	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(map[string]string{"path": cpPath})
}

func (s *apiServer) handleFork(w http.ResponseWriter, r *http.Request) {
	if s.sock == nil {
		http.Error(w, "guest vsock unavailable", http.StatusServiceUnavailable)
		return
	}
	if s.instanceName == "" || s.instanceDir == "" {
		http.Error(w, "fork requires daemon mode", http.StatusBadRequest)
		return
	}

	// Step 1: Tell guest to CRIU dump for fork.
	var dumpResp struct {
		Status string `json:"status"`
	}
	if err := s.proxyGuestJSON(r.Context(), "/criu/fork-dump", nil, &dumpResp); err != nil {
		http.Error(w, fmt.Sprintf("guest fork dump: %v", err), http.StatusBadGateway)
		return
	}

	// Step 2: Clone rootfs + CRIU volume + spawn child.
	resp := s.executeFork()
	if resp.Error != "" {
		http.Error(w, resp.Error, http.StatusInternalServerError)
		return
	}

	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(map[string]string{"child_instance": resp.Instance})
}

// copyForkMetadata copies instance metadata files relevant to a fork.
// Skips mac.addr, swap.img, hibernated, and other transient state.
func copyForkMetadata(srcDir, dstDir string) {
	// Only copy shares.json if it exists.
	sharesPath := filepath.Join(srcDir, "shares.json")
	if data, err := os.ReadFile(sharesPath); err == nil {
		os.WriteFile(filepath.Join(dstDir, "shares.json"), data, 0644)
	}
}

func (s *apiServer) handleCheckpoint(w http.ResponseWriter, r *http.Request) {
	var req struct {
		Name string `json:"name"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil && err != io.EOF {
		http.Error(w, "bad request", http.StatusBadRequest)
		return
	}

	if s.sock != nil {
		var resp struct {
			Path string `json:"path"`
		}
		if err := s.proxyGuestJSON(r.Context(), "/checkpoint", req, &resp); err != nil {
			// Guest sync failed — fall through to direct clone without sync.
			slog.Warn("checkpoint: guest sync failed, cloning without sync", "error", err)
		} else {
			w.Header().Set("Content-Type", "application/json")
			_ = json.NewEncoder(w).Encode(resp)
			return
		}
	}

	cpDir := filepath.Join(filepath.Dir(s.rootfsPath), "checkpoints")
	cpPath, err := CreateCheckpoint(s.rootfsPath, cpDir, req.Name)
	if err != nil {
		http.Error(w, err.Error(), http.StatusConflict)
		return
	}
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(map[string]string{"path": cpPath})
}

func (s *apiServer) proxyGuestJSON(ctx context.Context, path string, body any, respBody any) error {
	payload, err := json.Marshal(body)
	if err != nil {
		return fmt.Errorf("marshal guest request: %w", err)
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, "http://guest"+path, strings.NewReader(string(payload)))
	if err != nil {
		return fmt.Errorf("build guest request: %w", err)
	}
	req.Header.Set("Content-Type", "application/json")

	client := &http.Client{
		Transport: &http.Transport{
			DialContext: func(_ context.Context, _, _ string) (net.Conn, error) {
				return s.sock.Connect(protocol.GuestHTTPPort)
			},
		},
	}

	resp, err := client.Do(req)
	if err != nil {
		if errSuggestsDeadVM(err) {
			s.requestStop("vm no longer live during guest request", "path", path, "error", err)
		}
		return fmt.Errorf("guest request failed: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode/100 != 2 {
		data, _ := io.ReadAll(resp.Body)
		msg := strings.TrimSpace(string(data))
		if msg == "" {
			msg = resp.Status
		}
		return fmt.Errorf("guest request failed: %s", msg)
	}
	if respBody != nil {
		if err := json.NewDecoder(resp.Body).Decode(respBody); err != nil && err != io.EOF {
			return fmt.Errorf("decode guest response: %w", err)
		}
	}
	return nil
}

func errSuggestsDeadVM(err error) bool {
	if err == nil {
		return false
	}
	s := err.Error()
	return strings.Contains(s, "Invalid virtual machine state") ||
		strings.Contains(s, "no longer live")
}

// ExecRequest is the JSON body for POST /exec and the first WebSocket text frame.
type ExecRequest struct {
	Args      []string `json:"args"`
	Env       []string `json:"env,omitempty"`
	PTY       bool     `json:"pty,omitempty"`
	Rows      uint16   `json:"rows,omitempty"`
	Cols      uint16   `json:"cols,omitempty"`
	ClientPID int      `json:"client_pid,omitempty"`
}

func (s *apiServer) handleExec(w http.ResponseWriter, r *http.Request) {
	var req ExecRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "bad request: "+err.Error(), http.StatusBadRequest)
		return
	}
	if len(req.Args) == 0 {
		http.Error(w, "args required", http.StatusBadRequest)
		return
	}

	s.execStarted()
	defer s.execFinished()

	execEnc, execDec, execConn, err := s.connectExec()
	if err != nil {
		http.Error(w, "guest exec connect: "+err.Error(), http.StatusServiceUnavailable)
		return
	}
	defer execConn.Close()

	sessID := s.registerSession(req.Args, req.PTY, req.ClientPID, execEnc, execConn)
	defer s.unregisterSession(sessID)

	if err := execEnc.Encode(protocol.Msg{ExecReq: &protocol.ExecReq{
		Args: req.Args,
		Env:  req.Env,
		PTY:  req.PTY,
		Rows: req.Rows,
		Cols: req.Cols,
	}}); err != nil {
		http.Error(w, "send exec request: "+err.Error(), http.StatusInternalServerError)
		return
	}

	if req.PTY {
		http.Error(w, "use GET /exec/ws for interactive exec", http.StatusBadRequest)
		return
	}
	s.handleExecStream(w, execDec, sessID)
}

// handleExecStream handles non-interactive exec via NDJSON streaming.
func (s *apiServer) handleExecStream(w http.ResponseWriter, execDec *gob.Decoder, sessID string) {
	flusher, ok := w.(http.Flusher)
	if !ok {
		http.Error(w, "streaming not supported", http.StatusInternalServerError)
		return
	}

	w.Header().Set("Content-Type", "application/x-ndjson")
	w.WriteHeader(http.StatusOK)
	flusher.Flush()

	enc := json.NewEncoder(w)
	for {
		var msg protocol.Msg
		if err := execDec.Decode(&msg); err != nil {
			if err != io.EOF {
				slog.Debug("exec decode failed", "error", err)
			}
			enc.Encode(map[string]int{"exit_code": -1})
			flusher.Flush()
			return
		}

		if msg.ExecStarted != nil {
			s.setSessionGuestPID(sessID, msg.ExecStarted.PID)
		}

		if msg.ExecOutput != nil {
			if len(msg.ExecOutput.Stdout) > 0 {
				enc.Encode(map[string]string{"stdout": string(msg.ExecOutput.Stdout)})
				flusher.Flush()
			}
			if len(msg.ExecOutput.Stderr) > 0 {
				enc.Encode(map[string]string{"stderr": string(msg.ExecOutput.Stderr)})
				flusher.Flush()
			}
		}

		if msg.ForkNotify != nil {
			enc.Encode(map[string]any{
				"fork": map[string]string{"instance": msg.ForkNotify.Instance},
			})
			flusher.Flush()
		}

		if msg.ExecDone != nil {
			enc.Encode(map[string]int{"exit_code": msg.ExecDone.ExitCode})
			flusher.Flush()
			return
		}
	}
}

// handleExecWS handles interactive exec over WebSocket.
// Binary frames carry raw PTY data. Text frames carry JSON control messages:
//
//	Client → Server: {"signal": N} or {"resize": {"rows": R, "cols": C}}
//	Server → Client: {"exit_code": N}
func (s *apiServer) handleExecWS(w http.ResponseWriter, r *http.Request) {
	ws, err := websocket.Accept(w, r, &websocket.AcceptOptions{
		// Allow any origin since this is a local unix socket.
		InsecureSkipVerify: true,
	})
	if err != nil {
		slog.Debug("websocket accept failed", "error", err)
		return
	}
	defer ws.CloseNow()
	ws.SetReadLimit(-1) // no limit on PTY data

	ctx := r.Context()

	// Read the first text message as the ExecRequest.
	_, data, err := ws.Read(ctx)
	if err != nil {
		slog.Debug("websocket read exec request failed", "error", err)
		return
	}
	var req ExecRequest
	if err := json.Unmarshal(data, &req); err != nil {
		ws.Close(websocket.StatusInvalidFramePayloadData, "bad exec request: "+err.Error())
		return
	}
	if len(req.Args) == 0 {
		ws.Close(websocket.StatusInvalidFramePayloadData, "args required")
		return
	}

	s.execStarted()
	defer s.execFinished()

	execEnc, execDec, execConn, err := s.connectExec()
	if err != nil {
		ws.Close(websocket.StatusInternalError, "guest exec connect: "+err.Error())
		return
	}
	defer execConn.Close()

	sessID := s.registerSession(req.Args, true, req.ClientPID, execEnc, execConn)
	defer s.unregisterSession(sessID)

	// Look up the session for thread-safe encoder access.
	s.sessionsMu.RLock()
	sess := s.sessions[sessID]
	s.sessionsMu.RUnlock()

	if err := sess.encodeExec(protocol.Msg{ExecReq: &protocol.ExecReq{
		Args: req.Args,
		Env:  req.Env,
		PTY:  true,
		Rows: req.Rows,
		Cols: req.Cols,
	}}); err != nil {
		ws.Close(websocket.StatusInternalError, "send exec request: "+err.Error())
		return
	}

	// Connect to guest PTY via vsock.
	var vsockConn net.Conn
	for i := 0; i < 300; i++ {
		vsockConn, err = s.sock.Connect(protocol.ExecInteractivePort)
		if err == nil {
			break
		}
		time.Sleep(200 * time.Millisecond)
	}
	if vsockConn == nil {
		ws.Close(websocket.StatusInternalError, "exec interactive connect failed")
		return
	}
	defer vsockConn.Close()
	s.setSessionVsockConn(sessID, vsockConn)

	// Read text frames (signals/resize) from client and forward to guest gob connection.
	// Read binary frames (stdin) from client and write to guest PTY vsock.
	go func() {
		for {
			typ, data, err := ws.Read(ctx)
			if err != nil {
				// Client disconnected — close both the PTY vsock and the
				// exec gob connection so the guest cleans up and
				// execDec.Decode() unblocks.
				vsockConn.Close()
				execConn.Close()
				return
			}
			switch typ {
			case websocket.MessageBinary:
				vsockConn.Write(data)
			case websocket.MessageText:
				var ctrl wsControl
				if err := json.Unmarshal(data, &ctrl); err != nil {
					continue
				}
				if ctrl.Signal != nil {
					sess.encodeExec(protocol.Msg{ExecSignal: &protocol.ExecSignal{Sig: *ctrl.Signal}})
				}
				if ctrl.Resize != nil {
					sess.encodeExec(protocol.Msg{ExecResize: &protocol.ExecResize{
						Rows: ctrl.Resize.Rows,
						Cols: ctrl.Resize.Cols,
					}})
				}
			}
		}
	}()

	// Read guest PTY output and send as binary frames.
	go func() {
		buf := make([]byte, 32*1024)
		for {
			n, err := vsockConn.Read(buf)
			if n > 0 {
				if werr := ws.Write(ctx, websocket.MessageBinary, buf[:n]); werr != nil {
					return
				}
			}
			if err != nil {
				return
			}
		}
	}()

	// Wait for ExecStarted then ExecDone from guest.
	exitCode := -1
	for {
		var msg protocol.Msg
		if err := execDec.Decode(&msg); err != nil {
			break
		}
		if msg.ExecStarted != nil {
			s.setSessionGuestPID(sessID, msg.ExecStarted.PID)
		}
		if msg.ExecOutput != nil {
			if len(msg.ExecOutput.Stdout) > 0 {
				if err := ws.Write(ctx, websocket.MessageBinary, terminalizeNewlines(msg.ExecOutput.Stdout)); err != nil {
					break
				}
			}
			if len(msg.ExecOutput.Stderr) > 0 {
				if err := ws.Write(ctx, websocket.MessageBinary, terminalizeNewlines(msg.ExecOutput.Stderr)); err != nil {
					break
				}
			}
		}
		if msg.ForkNotify != nil {
			forkMsg, _ := json.Marshal(map[string]any{
				"fork": map[string]string{"instance": msg.ForkNotify.Instance},
			})
			ws.Write(ctx, websocket.MessageText, forkMsg)
		}
		if msg.ExecDone != nil {
			exitCode = msg.ExecDone.ExitCode
			break
		}
	}

	// Send exit code to client as text frame.
	exitMsg, _ := json.Marshal(map[string]int{"exit_code": exitCode})
	ws.Write(ctx, websocket.MessageText, exitMsg)
	ws.Close(websocket.StatusNormalClosure, "")
}

// handleForkAttachWS connects to a CRIU-restored fork session's PTY in the
// guest and bridges it to the client WebSocket. The protocol is identical
// to handleExecWS (binary = PTY data, text = signals/resize/exit_code)
// except the guest side is a fork attach server rather than an exec server.
func (s *apiServer) handleForkAttachWS(w http.ResponseWriter, r *http.Request) {
	ws, err := websocket.Accept(w, r, &websocket.AcceptOptions{
		InsecureSkipVerify: true,
	})
	if err != nil {
		slog.Debug("fork attach websocket accept failed", "error", err)
		return
	}
	defer ws.CloseNow()
	ws.SetReadLimit(-1)

	ctx := r.Context()

	// Read the first text message for PTY dimensions.
	_, data, err := ws.Read(ctx)
	if err != nil {
		slog.Debug("fork attach websocket read request failed", "error", err)
		return
	}
	var req ExecRequest
	if err := json.Unmarshal(data, &req); err != nil {
		ws.Close(websocket.StatusInvalidFramePayloadData, "bad request: "+err.Error())
		return
	}

	s.execStarted()
	defer s.execFinished()

	// Connect to the guest's fork attach gob port.
	var gobConn net.Conn
	for i := 0; i < 300; i++ {
		gobConn, err = s.sock.Connect(protocol.ForkAttachPort)
		if err == nil {
			break
		}
		if errSuggestsDeadVM(err) {
			ws.Close(websocket.StatusInternalError, "vm not live")
			return
		}
		time.Sleep(200 * time.Millisecond)
	}
	if gobConn == nil {
		ws.Close(websocket.StatusInternalError, "fork attach connect failed")
		return
	}
	defer gobConn.Close()

	gobEnc := gob.NewEncoder(gobConn)
	gobDec := gob.NewDecoder(gobConn)

	sessID := s.registerSession([]string{"[fork]"}, true, req.ClientPID, gobEnc, gobConn)
	defer s.unregisterSession(sessID)

	// Send ExecReq with PTY dimensions so the guest can resize.
	if err := gobEnc.Encode(protocol.Msg{ExecReq: &protocol.ExecReq{
		PTY:  true,
		Rows: req.Rows,
		Cols: req.Cols,
	}}); err != nil {
		ws.Close(websocket.StatusInternalError, "send fork request: "+err.Error())
		return
	}

	// Connect to the guest's fork attach data port.
	var dataConn net.Conn
	for i := 0; i < 300; i++ {
		dataConn, err = s.sock.Connect(protocol.ForkAttachDataPort)
		if err == nil {
			break
		}
		time.Sleep(200 * time.Millisecond)
	}
	if dataConn == nil {
		ws.Close(websocket.StatusInternalError, "fork attach data connect failed")
		return
	}
	defer dataConn.Close()
	s.setSessionVsockConn(sessID, dataConn)

	// Look up session for thread-safe encoder access.
	s.sessionsMu.RLock()
	sess := s.sessions[sessID]
	s.sessionsMu.RUnlock()

	// Client → guest: text = signals/resize, binary = stdin.
	go func() {
		for {
			typ, data, err := ws.Read(ctx)
			if err != nil {
				dataConn.Close()
				gobConn.Close()
				return
			}
			switch typ {
			case websocket.MessageBinary:
				dataConn.Write(data)
			case websocket.MessageText:
				var ctrl wsControl
				if err := json.Unmarshal(data, &ctrl); err != nil {
					continue
				}
				if ctrl.Signal != nil {
					sess.encodeExec(protocol.Msg{ExecSignal: &protocol.ExecSignal{Sig: *ctrl.Signal}})
				}
				if ctrl.Resize != nil {
					sess.encodeExec(protocol.Msg{ExecResize: &protocol.ExecResize{
						Rows: ctrl.Resize.Rows,
						Cols: ctrl.Resize.Cols,
					}})
				}
			}
		}
	}()

	// Guest PTY → client.
	go func() {
		buf := make([]byte, 32*1024)
		for {
			n, err := dataConn.Read(buf)
			if n > 0 {
				if werr := ws.Write(ctx, websocket.MessageBinary, buf[:n]); werr != nil {
					return
				}
			}
			if err != nil {
				return
			}
		}
	}()

	// Wait for ExecStarted/ExecDone from guest gob connection.
	exitCode := -1
	for {
		var msg protocol.Msg
		if err := gobDec.Decode(&msg); err != nil {
			break
		}
		if msg.ExecStarted != nil {
			s.setSessionGuestPID(sessID, msg.ExecStarted.PID)
		}
		if msg.ForkNotify != nil {
			forkMsg, _ := json.Marshal(map[string]any{
				"fork": map[string]string{"instance": msg.ForkNotify.Instance},
			})
			ws.Write(ctx, websocket.MessageText, forkMsg)
		}
		if msg.ExecDone != nil {
			exitCode = msg.ExecDone.ExitCode
			break
		}
	}

	exitMsg, _ := json.Marshal(map[string]int{"exit_code": exitCode})
	ws.Write(ctx, websocket.MessageText, exitMsg)
	ws.Close(websocket.StatusNormalClosure, "")
}

// wsControl is the JSON structure for WebSocket text frames from client.
type wsControl struct {
	Signal *int      `json:"signal,omitempty"`
	Resize *wsResize `json:"resize,omitempty"`
}

type wsResize struct {
	Rows uint16 `json:"rows"`
	Cols uint16 `json:"cols"`
}

func terminalizeNewlines(data []byte) []byte {
	if !bytes.Contains(data, []byte{'\n'}) {
		return data
	}
	var out []byte
	out = make([]byte, 0, len(data)+8)
	for i, b := range data {
		if b == '\n' && (i == 0 || data[i-1] != '\r') {
			out = append(out, '\r', '\n')
			continue
		}
		out = append(out, b)
	}
	return out
}

// handleSessions returns all active exec sessions as JSON, sorted by start time.
func (s *apiServer) handleSessions(w http.ResponseWriter, r *http.Request) {
	s.sessionsMu.RLock()
	sessions := make([]*SessionInfo, 0, len(s.sessions))
	for _, sess := range s.sessions {
		sessions = append(sessions, sess)
	}
	s.sessionsMu.RUnlock()

	sort.Slice(sessions, func(i, j int) bool {
		return sessions[i].StartTime.Before(sessions[j].StartTime)
	})

	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(sessions)
}

// SessionKillRequest is the JSON body for POST /sessions/kill.
type SessionKillRequest struct {
	ID     string `json:"id"`
	Signal int    `json:"signal"`
	Close  bool   `json:"close,omitempty"` // also close connections to force teardown
}

// handleSessionKill sends a signal to a session's guest process.
// If Close is true, also forcefully closes the session's connections.
func (s *apiServer) handleSessionKill(w http.ResponseWriter, r *http.Request) {
	var req SessionKillRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "bad request: "+err.Error(), http.StatusBadRequest)
		return
	}
	if req.ID == "" {
		http.Error(w, "id required", http.StatusBadRequest)
		return
	}
	if req.Signal == 0 {
		req.Signal = 15 // SIGTERM
	}

	if !s.signalSession(req.ID, req.Signal) {
		http.Error(w, "session not found: "+req.ID, http.StatusNotFound)
		return
	}

	if req.Close {
		s.closeSession(req.ID)
	}

	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(map[string]string{"status": "signal sent"})
}

// handleStop shuts down the VM daemon.
func (s *apiServer) handleStop(w http.ResponseWriter, r *http.Request) {
	s.requestStop("stop requested by client")
	w.WriteHeader(http.StatusOK)
	fmt.Fprintln(w, "stopping")
}

// runExec runs a command on the guest. Creates a new vsock exec connection
// per call so multiple execs can run concurrently.
// For interactive (PTY) mode, it splices os.Stdin/os.Stdout with the guest PTY.
// forceQuitCh is passed to spliceInteractive for raw-mode double-Ctrl-C detection.
// Returns the exit code.
func (s *apiServer) runExec(req *protocol.ExecReq, interactive bool, forceQuitCh chan struct{}) int {
	execEnc, execDec, execConn, err := s.connectExec()
	if err != nil {
		slog.Info("exec connect failed", "error", err)
		return -1
	}
	defer execConn.Close()

	s.execStarted()
	defer s.execFinished()

	if err := execEnc.Encode(protocol.Msg{ExecReq: req}); err != nil {
		slog.Debug("exec encode failed", "error", err)
		return -1
	}

	if interactive {
		// Wrap os.Stdin/os.Stdout as a ReadWriter for spliceInteractive.
		return s.spliceInteractive(readWriter{os.Stdin, os.Stdout}, execDec, forceQuitCh)
	}
	return readExecOutput(execDec, os.Stdout, os.Stderr)
}

// readWriter pairs a reader and writer into an io.ReadWriter.
type readWriter struct {
	io.Reader
	io.Writer
}

// readExecOutput reads ExecOutput/ExecDone from the gob connection,
// writing stdout/stderr to the provided writers.
func readExecOutput(execDec *gob.Decoder, stdout, stderr io.Writer) int {
	for {
		var msg protocol.Msg
		if err := execDec.Decode(&msg); err != nil {
			return -1
		}
		if msg.ExecOutput != nil {
			if len(msg.ExecOutput.Stdout) > 0 {
				stdout.Write(msg.ExecOutput.Stdout)
			}
			if len(msg.ExecOutput.Stderr) > 0 {
				stderr.Write(msg.ExecOutput.Stderr)
			}
		}
		if msg.ExecDone != nil {
			return msg.ExecDone.ExitCode
		}
	}
}

// spliceInteractive connects to the guest PTY via vsock and splices
// raw bytes between rw (stdin/stdout or hijacked HTTP conn) and the PTY.
// Used by both the main command and `lnx exec -i`.
//
// forceQuitCh, if non-nil, is closed when a double Ctrl-C is detected in the
// raw byte stream (needed because term.MakeRaw disables ISIG).
func (s *apiServer) spliceInteractive(rw io.ReadWriter, execDec *gob.Decoder, forceQuitCh chan struct{}) int {
	var vsockConn net.Conn
	for i := 0; i < 300; i++ {
		var err error
		vsockConn, err = s.sock.Connect(protocol.ExecInteractivePort)
		if err == nil {
			break
		}
		time.Sleep(200 * time.Millisecond)
	}
	if vsockConn == nil {
		slog.Info("exec interactive connect failed")
		return -1
	}
	defer vsockConn.Close()

	reader := io.Reader(rw)
	if forceQuitCh != nil {
		reader = &ctrlCReader{r: rw, conn: vsockConn, forceQuitCh: forceQuitCh}
	}
	go io.Copy(vsockConn, reader)
	io.Copy(rw, vsockConn)

	// If force quit was triggered, don't wait for the exec done message —
	// the guest process may still be alive (e.g. trapping signals).
	if forceQuitCh != nil {
		select {
		case <-forceQuitCh:
			return -1
		default:
		}
	}

	var msg protocol.Msg
	if err := execDec.Decode(&msg); err == nil && msg.ExecDone != nil {
		return msg.ExecDone.ExitCode
	}
	return -1
}

// ctrlCReader wraps a reader and detects double Ctrl-C (0x03) in raw mode.
// When detected, it closes the vsock connection to force-quit the VM.
// Individual Ctrl-C bytes are still forwarded to the guest.
type ctrlCReader struct {
	r           io.Reader
	conn        net.Conn
	forceQuitCh chan struct{}
	lastCtrlC   time.Time
}

func (c *ctrlCReader) Read(p []byte) (int, error) {
	n, err := c.r.Read(p)
	for i := 0; i < n; i++ {
		if p[i] == 0x03 { // Ctrl-C
			now := time.Now()
			if !c.lastCtrlC.IsZero() && now.Sub(c.lastCtrlC) < time.Second {
				fmt.Fprintln(os.Stderr, "\r\nforce quit")
				close(c.forceQuitCh)
				c.conn.Close()
				return 0, io.EOF
			}
			c.lastCtrlC = now
		}
	}
	return n, err
}

func (s *apiServer) close() {
	if s.listener != nil {
		s.listener.Close()
	}
	if s.sockPath != "" {
		os.Remove(s.sockPath)
	}
	s.statusMu.Lock()
	if s.statusConn != nil {
		s.statusConn.Close()
	}
	s.statusMu.Unlock()
	if s.guestCtrlConn != nil {
		s.guestCtrlConn.Close()
	}
}
