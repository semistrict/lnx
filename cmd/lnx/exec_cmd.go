package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"strings"

	"github.com/semistrict/lnx"
	"github.com/spf13/cobra"
)

var execCmd = &cobra.Command{
	Use:                "exec [flags] -- command [args...]",
	Short:              "Run a command in the running VM",
	DisableFlagParsing: true,
	RunE:               runExec,
}

func init() {
	rootCmd.AddCommand(execCmd)
}

func runExec(cmd *cobra.Command, args []string) error {
	// Strip leading "--" if present.
	if len(args) > 0 && args[0] == "--" {
		args = args[1:]
	}
	if len(args) == 0 {
		return fmt.Errorf("usage: lnx exec [--] command [args...]")
	}

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

	// Read NDJSON stream.
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
