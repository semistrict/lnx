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

	vz "github.com/Code-Hex/vz/v3"

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

	guestCtrlConn net.Conn

	sock *vz.VirtioSocketDevice
	pf   *portForwarder

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
	mux.HandleFunc("GET /ports", s.handlePorts)
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

// PortEntry describes a single forwarded port.
type PortEntry struct {
	Guest uint16 `json:"guest"`
	Host  uint16 `json:"host"`
}

func (s *apiServer) handlePorts(w http.ResponseWriter, r *http.Request) {
	var ports []PortEntry
	if s.pf != nil {
		s.pf.mu.Lock()
		for _, fp := range s.pf.listeners {
			ports = append(ports, PortEntry{Guest: fp.guestPort, Host: fp.hostPort})
		}
		s.pf.mu.Unlock()
	}
	if ports == nil {
		ports = []PortEntry{}
	}
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(ports)
}

// ExecRequest is the JSON body for POST /exec.
type ExecRequest struct {
	Args []string `json:"args"`
	Env  []string `json:"env,omitempty"`
	PTY  bool     `json:"pty,omitempty"`
	Rows uint16   `json:"rows,omitempty"`
	Cols uint16   `json:"cols,omitempty"`
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

	execEnc, execDec, execConn, err := s.connectExec()
	if err != nil {
		http.Error(w, "guest exec connect: "+err.Error(), http.StatusServiceUnavailable)
		return
	}
	defer execConn.Close()

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
		s.handleExecInteractive(w, r, execDec)
	} else {
		s.handleExecStream(w, execDec)
	}
}

// handleExecStream handles non-interactive exec via NDJSON streaming.
func (s *apiServer) handleExecStream(w http.ResponseWriter, execDec *gob.Decoder) {
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

// handleExecInteractive handles interactive exec with PTY via HTTP hijack.
func (s *apiServer) handleExecInteractive(w http.ResponseWriter, r *http.Request, execDec *gob.Decoder) {
	hj, ok := w.(http.Hijacker)
	if !ok {
		http.Error(w, "hijack not supported", http.StatusInternalServerError)
		return
	}
	conn, buf, err := hj.Hijack()
	if err != nil {
		slog.Debug("exec hijack failed", "error", err)
		return
	}
	defer conn.Close()

	buf.WriteString("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\r\n")
	buf.Flush()

	s.spliceInteractive(conn, execDec, nil)
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
