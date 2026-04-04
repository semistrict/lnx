// Package protocol defines the gob-encoded control messages exchanged
// between the host (macOS) and guest (Linux) over vsock port 1024.
package protocol

const (
	// Port is the vsock port used for the control connection.
	Port = 1024
	// StatusPort is the vsock port for status queries.
	StatusPort = 1026
	// ExecPort is the vsock port for exec requests.
	ExecPort = 1027
	// GuestControlPort is the vsock port for guest-initiated requests (checkpoint, etc).
	GuestControlPort = 1028
	// TerminalPort is the vsock port for raw terminal I/O (stdin/stdout bytes).
	TerminalPort = 1029
)

// Msg is the envelope for all control messages.
// Exactly one field is non-nil per message.
type Msg struct {
	Exec       *Exec
	Signal     *Signal
	Exit       *Exit
	Ack        *Ack
	Resize     *Resize
	StatusReq  *StatusReq
	StatusResp *StatusResp
	ExecReq       *ExecReq
	ExecOutput    *ExecOutput
	ExecDone      *ExecDone
	CheckpointReq  *CheckpointReq
	CheckpointResp *CheckpointResp
	OpenURLReq     *OpenURLReq
	OpenURLResp    *OpenURLResp
}

// Exec tells the guest to run a command.
type Exec struct {
	Args    []string // command vector: Args[0] is the program
	CWD     string
	Env     []string // KEY=VALUE pairs
	PTY     bool
	User    string // guest username (matches host)
	UID     int    // guest UID (matches host)
	HomeDir string // host home dir path (e.g. /Users/ramon), mounted read-only
	Rows    uint16 // initial terminal rows (0 = unknown)
	Cols    uint16 // initial terminal cols (0 = unknown)
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

// Exit reports the guest process exit code back to the host.
type Exit struct {
	Code int
}

// Ack confirms the host received the guest's Exit message.
type Ack struct{}

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
