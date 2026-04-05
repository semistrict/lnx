// Package protocol defines the gob-encoded control messages exchanged
// between the host (macOS) and guest (Linux) over vsock.
package protocol

const (
	// Port is the vsock port used for the control connection
	// (setup, signals, resize).
	Port = 1024
	// StatusPort is the vsock port for status queries.
	StatusPort = 1026
	// ExecPort is the vsock port for exec requests. The guest listens;
	// the host connects once per exec session via VirtioSocketDevice.Connect.
	ExecPort = 1027
	// GuestControlPort is the vsock port for guest-initiated requests (checkpoint, etc).
	GuestControlPort = 1028
	// PortForwardPort is the vsock port for port-forwarding notifications (guest → host).
	PortForwardPort = 1030
	// PortForwardDataPort is the vsock port the guest listens on for
	// forwarded TCP connections (host connects via VirtioSocketDevice.Connect).
	PortForwardDataPort = 1031
	// ExecInteractivePort is the vsock port the guest listens on for
	// interactive exec PTY connections (host connects via VirtioSocketDevice.Connect).
	ExecInteractivePort = 1032
	// P9Port is the vsock port for the 9P file server (host listens, guest dials).
	P9Port = 1033
	// SSHAgentPort is the vsock port for SSH agent forwarding (host listens, guest dials).
	SSHAgentPort = 1034
)

// Msg is the envelope for all control messages.
// Exactly one field is non-nil per message.
type Msg struct {
	Setup          *Setup
	Signal         *Signal
	Resize         *Resize
	StatusReq      *StatusReq
	StatusResp     *StatusResp
	ExecReq        *ExecReq
	ExecStarted    *ExecStarted
	ExecOutput     *ExecOutput
	ExecDone       *ExecDone
	ExecSignal     *ExecSignal
	ExecResize     *ExecResize
	CheckpointReq  *CheckpointReq
	CheckpointResp *CheckpointResp
	OpenURLReq     *OpenURLReq
	OpenURLResp    *OpenURLResp
}

// Setup tells the guest the environment to configure (user, cwd, env vars).
// Sent once on the control connection at boot. No command args — commands
// are executed via the exec connection.
type Setup struct {
	CWD      string
	Env      []string // KEY=VALUE pairs
	User     string   // guest username (matches host)
	UID      int      // guest UID (matches host)
	HomeDir  string   // host home dir path (e.g. /Users/ramon), mounted read-only
	Hostname string   // guest hostname (e.g. "default.lnx")
	SSHAgent bool     // if true, host is forwarding SSH agent on SSHAgentPort
	GUI      bool     // if true, a virtio-gpu display is attached — start a compositor
	Shares   []string // extra virtiofs shares to mount read-write (absolute paths)
}

// Resize tells the guest to update the PTY window size.
type Resize struct {
	Rows uint16
	Cols uint16
}

// Signal tells the guest to forward a signal to the running process.
type Signal struct {
	Sig int // syscall.Signal value
}

// StatusReq asks the guest for current system status.
type StatusReq struct {
	IncludeDmesg bool
}

// StatusResp reports guest system status.
type StatusResp struct {
	UptimeSecs  float64
	MemTotalKB  uint64
	MemAvailKB  uint64
	SwapTotalKB uint64
	SwapFreeKB  uint64
	DiskTotalKB uint64
	DiskUsedKB  uint64
	LoadAvg     string
	Dmesg       string // only populated if StatusReq.IncludeDmesg was true
}

// ExecReq asks the guest to run a command.
type ExecReq struct {
	Args []string
	Env  []string
	CWD  string // working directory (empty = use setup CWD)
	PTY  bool
	Rows uint16
	Cols uint16
}

// ExecStarted is sent by the guest after a command starts, reporting the guest PID.
// Sent on the per-session exec gob connection (port 1027).
type ExecStarted struct {
	PID int
}

// ExecOutput streams command output from guest to host.
type ExecOutput struct {
	Stdout []byte
	Stderr []byte
}

// ExecDone reports that the exec command has finished.
type ExecDone struct {
	ExitCode int
}

// CheckpointReq asks the host to snapshot the rootfs.
type CheckpointReq struct{}

// CheckpointResp reports the result of a checkpoint.
type CheckpointResp struct {
	Path  string // path of the checkpoint on the host
	Error string // non-empty on failure
}

// OpenURLReq asks the host to open a URL in the default browser.
type OpenURLReq struct {
	URL string
}

// OpenURLResp reports the result of opening a URL.
type OpenURLResp struct {
	Error string // non-empty on failure
}

// ExecSignal tells the guest to forward a signal to a specific exec session's process.
// Sent on the per-session exec gob connection (port 1027).
type ExecSignal struct {
	Sig int // syscall.Signal value
}

// ExecResize tells the guest to resize a specific exec session's PTY.
// Sent on the per-session exec gob connection (port 1027).
type ExecResize struct {
	Rows uint16
	Cols uint16
}

// PortForward notifies the host of the current set of listening TCP ports in the guest.
type PortForward struct {
	Ports []uint16
}
