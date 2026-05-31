package main

import (
	"bufio"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/spf13/cobra"
)

var sshProxyCmd = &cobra.Command{
	Use:    "_ssh-proxy hostname [port]",
	Short:  "SSH ProxyCommand helper (internal)",
	Hidden: true,
	Args:   cobra.RangeArgs(1, 2),
	RunE:   runSSHProxy,
}

func init() {
	rootCmd.AddCommand(sshProxyCmd)
}

func runSSHProxy(cmd *cobra.Command, args []string) error {
	hostname := args[0]
	// Strip .lnx suffix to get instance name.
	inst := strings.TrimSuffix(hostname, ".lnx")
	if inst == hostname {
		// No .lnx suffix — use as-is.
		inst = hostname
	}
	instanceName = inst
	instanceFlag = true

	if err := ensureVMRunning(); err != nil {
		return err
	}

	// Connect to the daemon and request the SSH proxy endpoint.
	// The guest SSH server may not be listening yet right after boot,
	// so retry on 502 (Bad Gateway) for a few seconds.
	conn, br, err := dialSSHProxy()
	if err != nil {
		return err
	}
	defer conn.Close()

	// The connection is now raw. Splice stdin/stdout with it.
	done := make(chan struct{})
	go func() {
		io.Copy(conn, os.Stdin)
		if tc, ok := conn.(*net.UnixConn); ok {
			tc.CloseWrite()
		}
		close(done)
	}()

	// Drain buffered data from the reader, then read directly from conn.
	if br.Buffered() > 0 {
		io.CopyN(os.Stdout, br, int64(br.Buffered()))
	}
	io.Copy(os.Stdout, conn)
	<-done

	return nil
}

// dialSSHProxy connects to the daemon's /ssh endpoint, retrying on 502
// while the guest SSH server finishes starting up.
func dialSSHProxy() (net.Conn, *bufio.Reader, error) {
	deadline := time.Now().Add(10 * time.Second)
	for {
		conn, br, err := tryDialSSHProxy()
		if err == nil {
			return conn, br, nil
		}
		if !strings.Contains(err.Error(), "502") || time.Now().After(deadline) {
			return nil, nil, err
		}
		time.Sleep(200 * time.Millisecond)
	}
}

// tryDialSSHProxy makes a single attempt to connect to the daemon's /ssh endpoint.
func tryDialSSHProxy() (net.Conn, *bufio.Reader, error) {
	var conn net.Conn
	var dialErr error
	for _, sp := range statusSockPaths() {
		conn, dialErr = net.Dial("unix", sp)
		if dialErr == nil {
			break
		}
	}
	if conn == nil {
		return nil, nil, fmt.Errorf("connect to VM daemon: %w", dialErr)
	}

	fmt.Fprintf(conn, "GET /ssh HTTP/1.1\r\nHost: localhost\r\n\r\n")

	br := bufio.NewReader(conn)
	statusLine, err := br.ReadString('\n')
	if err != nil {
		conn.Close()
		return nil, nil, fmt.Errorf("read ssh proxy response: %w", err)
	}
	if !strings.Contains(statusLine, "200") {
		conn.Close()
		return nil, nil, fmt.Errorf("ssh proxy failed: %s", strings.TrimSpace(statusLine))
	}

	// Skip remaining headers until blank line.
	for {
		line, err := br.ReadString('\n')
		if err != nil {
			conn.Close()
			return nil, nil, fmt.Errorf("read ssh proxy headers: %w", err)
		}
		if strings.TrimSpace(line) == "" {
			break
		}
	}

	return conn, br, nil
}

const sshConfigBlock = `# lnx: ssh into lnx VMs via "ssh <instance>.lnx"
Host *.lnx
  ProxyCommand lnx _ssh-proxy %h %p
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
# end lnx
`

// installSSHConfig adds the *.lnx Host block to ~/.ssh/config if not already present.
func installSSHConfig() {
	home, err := os.UserHomeDir()
	if err != nil {
		return
	}
	sshDir := filepath.Join(home, ".ssh")
	configPath := filepath.Join(sshDir, "config")

	// Check if already installed.
	if data, err := os.ReadFile(configPath); err == nil {
		if strings.Contains(string(data), "*.lnx") {
			return
		}
	}

	os.MkdirAll(sshDir, 0700)

	f, err := os.OpenFile(configPath, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0600)
	if err != nil {
		fmt.Fprintf(os.Stderr, "  ssh config: could not update %s: %v\n", configPath, err)
		return
	}
	defer f.Close()

	// Add a newline before the block if the file doesn't end with one.
	if info, _ := f.Stat(); info.Size() > 0 {
		f.WriteString("\n")
	}
	f.WriteString(sshConfigBlock)
	fmt.Fprintf(os.Stderr, "  ssh config: added *.lnx to %s\n", configPath)
}
