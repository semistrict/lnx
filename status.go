package lnx

import (
	"encoding/gob"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/semistrict/lnx/internal/protocol"
)

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
	args       []string
	user       string
	startTime  time.Time
	rootfsPath string

	statusMu   sync.Mutex
	statusEnc  *gob.Encoder
	statusDec  *gob.Decoder
	statusConn net.Conn

	execMu   sync.Mutex
	execEnc  *gob.Encoder
	execDec  *gob.Decoder
	execConn net.Conn

	guestCtrlConn net.Conn

	sockPath string
	listener net.Listener
}

func newAPIServer(args []string, user, rootfsPath string) *apiServer {
	return &apiServer{
		args:       args,
		user:       user,
		rootfsPath: rootfsPath,
		startTime:  time.Now(),
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

// setExecConn stores the guest's exec vsock connection.
func (s *apiServer) setExecConn(conn net.Conn) {
	s.execMu.Lock()
	defer s.execMu.Unlock()
	s.execConn = conn
	s.execEnc = gob.NewEncoder(conn)
	s.execDec = gob.NewDecoder(conn)
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
			cpPath, err := checkpoint(s.rootfsPath, cpDir)
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
	}
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
	s.sockPath = sockPath
	s.listener = ln

	mux := http.NewServeMux()
	mux.HandleFunc("GET /status", s.handleStatus)
	mux.HandleFunc("POST /exec", s.handleExec)

	go http.Serve(ln, mux)
	return nil
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

// ExecRequest is the JSON body for POST /exec.
type ExecRequest struct {
	Args []string `json:"args"`
	Env  []string `json:"env,omitempty"`
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

	s.execMu.Lock()
	defer s.execMu.Unlock()

	if s.execEnc == nil {
		http.Error(w, "guest exec not connected", http.StatusServiceUnavailable)
		return
	}

	// Send ExecReq to guest.
	if err := s.execEnc.Encode(protocol.Msg{ExecReq: &protocol.ExecReq{
		Args: req.Args,
		Env:  req.Env,
	}}); err != nil {
		http.Error(w, "send exec request: "+err.Error(), http.StatusInternalServerError)
		return
	}

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
		if err := s.execDec.Decode(&msg); err != nil {
			if err != io.EOF {
				slog.Debug("exec decode failed", "error", err)
			}
			// Write an error exit code if we lose connection.
			enc.Encode(map[string]int{"exit_code": -1})
			flusher.Flush()
			return
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

		if msg.ExecDone != nil {
			enc.Encode(map[string]int{"exit_code": msg.ExecDone.ExitCode})
			flusher.Flush()
			return
		}
	}
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
	s.execMu.Lock()
	if s.execConn != nil {
		s.execConn.Close()
	}
	s.execMu.Unlock()
	if s.guestCtrlConn != nil {
		s.guestCtrlConn.Close()
	}
}
