package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"

	"github.com/semistrict/lnx"
	"github.com/spf13/cobra"
	"golang.org/x/term"
)

var execCmd = &cobra.Command{
	Use:   "exec [flags] -- command [args...]",
	Short: "Run a command in the running VM",
	Long: `Run a command in the running VM.

Flags:
  -i, --interactive   Allocate a PTY for interactive use`,
	DisableFlagParsing: true,
	RunE:               runExec,
}

func init() {
	rootCmd.AddCommand(execCmd)
}

func runExec(cmd *cobra.Command, args []string) error {
	if len(args) == 1 && (args[0] == "--help" || args[0] == "-h") {
		return cmd.Help()
	}

	// Parse -i/--interactive manually since DisableFlagParsing is true.
	interactive := false
	var filtered []string
	for _, a := range args {
		if a == "-i" || a == "--interactive" {
			interactive = true
		} else {
			filtered = append(filtered, a)
		}
	}
	args = filtered

	// Strip leading "--" if present.
	if len(args) > 0 && args[0] == "--" {
		args = args[1:]
	}
	if len(args) == 0 {
		return fmt.Errorf("usage: lnx exec [-i] [--] command [args...]")
	}

	if interactive {
		return runExecInteractive(args)
	}
	return runExecStream(args)
}

func runExecStream(args []string) error {
	body, err := json.Marshal(lnx.ExecRequest{Args: args})
	if err != nil {
		return err
	}

	resp, err := apiClient().Post("http://localhost/exec", "application/json", bytes.NewReader(body))
	if err != nil {
		if isNoVM(err) {
			fmt.Fprintln(os.Stderr, "no VM running")
			os.Exit(1)
		}
		return fmt.Errorf("connect to VM: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		var buf bytes.Buffer
		buf.ReadFrom(resp.Body)
		return fmt.Errorf("exec failed: %s", strings.TrimSpace(buf.String()))
	}

	scanner := bufio.NewScanner(resp.Body)
	scanner.Buffer(make([]byte, 1024*1024), 1024*1024)

	exitCode := -1
	for scanner.Scan() {
		line := scanner.Bytes()
		var msg map[string]json.RawMessage
		if err := json.Unmarshal(line, &msg); err != nil {
			continue
		}

		if raw, ok := msg["stdout"]; ok {
			var s string
			json.Unmarshal(raw, &s)
			os.Stdout.WriteString(s)
		}
		if raw, ok := msg["stderr"]; ok {
			var s string
			json.Unmarshal(raw, &s)
			os.Stderr.WriteString(s)
		}
		if raw, ok := msg["exit_code"]; ok {
			json.Unmarshal(raw, &exitCode)
		}
	}

	os.Exit(exitCode)
	return nil
}

func runExecInteractive(args []string) error {
	fd := int(os.Stdin.Fd())
	var rows, cols uint16
	if term.IsTerminal(fd) {
		w, h, err := term.GetSize(fd)
		if err == nil {
			rows = uint16(h)
			cols = uint16(w)
		}
		oldState, err := term.MakeRaw(fd)
		if err == nil {
			defer term.Restore(fd, oldState)
		}
	}

	body, err := json.Marshal(lnx.ExecRequest{
		Args: args,
		PTY:  true,
		Rows: rows,
		Cols: cols,
	})
	if err != nil {
		return err
	}

	// Connect to the unix socket directly for raw I/O.
	sockPath := filepath.Join(lnxDir(), "status.sock")
	conn, err := net.Dial("unix", sockPath)
	if err != nil {
		if isNoVM(err) {
			fmt.Fprintln(os.Stderr, "no VM running")
			os.Exit(1)
		}
		return fmt.Errorf("connect to VM: %w", err)
	}
	defer conn.Close()

	// Write HTTP request manually.
	fmt.Fprintf(conn, "POST /exec HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: %d\r\n\r\n", len(body))
	conn.Write(body)

	// Read HTTP response.
	br := bufio.NewReader(conn)
	resp, err := http.ReadResponse(br, nil)
	if err != nil {
		return fmt.Errorf("read response: %w", err)
	}
	if resp.StatusCode != http.StatusOK {
		data, _ := io.ReadAll(resp.Body)
		resp.Body.Close()
		return fmt.Errorf("exec failed: %s", strings.TrimSpace(string(data)))
	}

	// Connection is now raw — splice stdin/stdout.
	done := make(chan struct{})
	go func() {
		io.Copy(conn, os.Stdin)
		close(done)
	}()

	// Read from connection. The last byte is the exit code.
	var buf bytes.Buffer
	io.Copy(&buf, br)
	<-done

	exitCode := 255
	if buf.Len() > 0 {
		data := buf.Bytes()
		// Write all but the last byte (terminal output).
		if len(data) > 1 {
			os.Stdout.Write(data[:len(data)-1])
		}
		exitCode = int(data[len(data)-1])
	}

	os.Exit(exitCode)
	return nil
}
