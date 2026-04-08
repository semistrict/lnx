package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"sort"
	"strings"
	"syscall"
	"time"

	"github.com/semistrict/lnx"
	"github.com/spf13/cobra"
)

var sessionsCmd = &cobra.Command{
	Use:   "sessions",
	Short: "Manage exec sessions",
}

var sessionsListCmd = &cobra.Command{
	Use:   "list",
	Short: "List active exec sessions",
	RunE:  runSessionsList,
}

var sessionsKillCmd = &cobra.Command{
	Use:   "kill <id>",
	Short: "Kill an exec session (SIGTERM, then SIGKILL after 10s)",
	Args:  cobra.ExactArgs(1),
	RunE:  runSessionsKill,
}

func init() {
	sessionsCmd.AddCommand(sessionsListCmd)
	sessionsCmd.AddCommand(sessionsKillCmd)
	rootCmd.AddCommand(sessionsCmd)
}

func runSessionsList(cmd *cobra.Command, args []string) error {
	if instanceFlag {
		return runSessionsListOne(instanceName)
	}
	return runSessionsListAll()
}

func runSessionsListAll() error {
	running := runningInstances()
	if len(running) == 0 {
		fmt.Println("no VM running")
		return nil
	}

	type row struct {
		instance string
		session  lnx.SessionInfo
	}

	var rows []row
	for _, name := range running {
		sessions, err := fetchSessions(name)
		if err != nil {
			continue
		}
		for _, s := range sessions {
			rows = append(rows, row{instance: name, session: s})
		}
	}

	if len(rows) == 0 {
		fmt.Println("no active sessions")
		return nil
	}

	sort.Slice(rows, func(i, j int) bool {
		return rows[i].session.StartTime.Before(rows[j].session.StartTime)
	})

	t := newTable("INSTANCE", "ID", "COMMAND", "LOCAL PID", "REMOTE PID", "AGE")
	for _, r := range rows {
		id := r.instance + "_" + r.session.ID
		t.Row(r.instance, id, formatCommand(r.session.Args), formatPID(r.session.ClientPID), formatPID(r.session.GuestPID), formatAge(time.Since(r.session.StartTime)))
	}
	fmt.Println(t)
	return nil
}

func runSessionsListOne(name string) error {
	sessions, err := fetchSessions(name)
	if err != nil {
		if isNoVM(err) {
			fmt.Println("no VM running")
			return nil
		}
		return err
	}

	if len(sessions) == 0 {
		fmt.Println("no active sessions")
		return nil
	}

	sort.Slice(sessions, func(i, j int) bool {
		return sessions[i].StartTime.Before(sessions[j].StartTime)
	})

	t := newTable("ID", "COMMAND", "LOCAL PID", "REMOTE PID", "AGE")
	for _, s := range sessions {
		t.Row(s.ID, formatCommand(s.Args), formatPID(s.ClientPID), formatPID(s.GuestPID), formatAge(time.Since(s.StartTime)))
	}
	fmt.Println(t)
	return nil
}

// runSessionsKill sends SIGTERM to both local and remote process, waits 10s,
// then sends SIGKILL if the session is still alive.
func runSessionsKill(cmd *cobra.Command, args []string) error {
	id := args[0]

	// Parse "instance_sN" format to determine instance and session ID.
	inst, sessID := parseSessionID(id)
	clientPID := sessionClientPID(inst, sessID)

	client := apiClientFor(inst)

	// Send SIGTERM + SIGHUP to guest process. SIGHUP is needed because
	// interactive shells (bash) ignore SIGTERM by default.
	if err := sendSessionSignal(client, sessID, int(syscall.SIGTERM), false); err != nil {
		return err
	}
	sendSessionSignal(client, sessID, int(syscall.SIGHUP), false)

	fmt.Fprintf(os.Stderr, "sent SIGTERM to session %s\n", id)

	// Wait up to 10s, checking if the session is still alive.
	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		time.Sleep(500 * time.Millisecond)
		sessions, err := fetchSessions(inst)
		if err != nil {
			// VM shut down or unreachable — session is gone.
			terminateLocalClient(clientPID)
			return nil
		}
		found := false
		for _, s := range sessions {
			if s.ID == sessID {
				found = true
				break
			}
		}
		if !found {
			terminateLocalClient(clientPID)
			return nil
		}
	}

	// Session still alive — send SIGKILL and close connections.
	fmt.Fprintf(os.Stderr, "session %s still alive, sending SIGKILL\n", id)

	sendSessionSignal(client, sessID, int(syscall.SIGKILL), true)
	terminateLocalClient(clientPID)

	return nil
}

// parseSessionID splits "instance_sN" into (instance, "sN").
// Session IDs always start with "s", so we split on the last "_s".
// If there's no such separator, assumes the current --instance.
func parseSessionID(id string) (string, string) {
	if i := strings.LastIndex(id, "_s"); i >= 0 {
		return id[:i], id[i+1:]
	}
	return instanceName, id
}

func sendSessionSignal(client *http.Client, sessID string, sig int, closeConn bool) error {
	body, _ := json.Marshal(lnx.SessionKillRequest{ID: sessID, Signal: sig, Close: closeConn})
	resp, err := client.Post("http://localhost/sessions/kill", "application/json", bytes.NewReader(body))
	if err != nil {
		if isNoVM(err) {
			return nil // VM already gone
		}
		return fmt.Errorf("signal session: %w", err)
	}
	resp.Body.Close()
	if resp.StatusCode == 404 {
		return fmt.Errorf("session %s not found", sessID)
	}
	return nil
}

func sessionClientPID(instance, sessID string) int {
	sessions, err := fetchSessions(instance)
	if err != nil {
		return 0
	}
	for _, s := range sessions {
		if s.ID == sessID {
			return s.ClientPID
		}
	}
	return 0
}

func terminateLocalClient(pid int) {
	if pid <= 0 || pid == os.Getpid() {
		return
	}
	if !processExists(pid) {
		return
	}
	_ = syscall.Kill(pid, syscall.SIGTERM)
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if !processExists(pid) {
			return
		}
		time.Sleep(100 * time.Millisecond)
	}
	_ = syscall.Kill(pid, syscall.SIGKILL)
}

func processExists(pid int) bool {
	return syscall.Kill(pid, 0) == nil
}

func formatCommand(args []string) string {
	s := strings.Join(args, " ")
	if len(s) > 40 {
		return s[:37] + "..."
	}
	return s
}

func formatPID(pid int) string {
	if pid > 0 {
		return fmt.Sprintf("%d", pid)
	}
	return "-"
}

func formatAge(d time.Duration) string {
	if d < time.Minute {
		return fmt.Sprintf("%ds", int(d.Seconds()))
	}
	if d < time.Hour {
		return fmt.Sprintf("%dm%ds", int(d.Minutes()), int(d.Seconds())%60)
	}
	return fmt.Sprintf("%dh%dm", int(d.Hours()), int(d.Minutes())%60)
}

func fetchSessions(name string) ([]lnx.SessionInfo, error) {
	resp, err := apiClientFor(name).Get("http://localhost/sessions")
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	var sessions []lnx.SessionInfo
	if err := json.NewDecoder(resp.Body).Decode(&sessions); err != nil {
		return nil, fmt.Errorf("read sessions: %w", err)
	}
	return sessions, nil
}
