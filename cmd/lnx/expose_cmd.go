package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"strconv"
	"strings"
	"time"

	"github.com/semistrict/lnx"
	"github.com/spf13/cobra"
)

const guestHostGateway = "192.168.64.1"

var (
	exposeAs   string
	exposeWait bool
)

var exposeCmd = &cobra.Command{
	Use:   "expose SOURCE",
	Short: "Expose a port from one VM on the host or another VM",
	Args:  cobra.ExactArgs(1),
	RunE:  runExpose,
}

type exposeEndpoint struct {
	Instance string
	Port     uint16
	PortSet  bool
}

func init() {
	exposeCmd.Flags().StringVar(&exposeAs, "as", "", "host or VM destination ([vm][:port])")
	exposeCmd.Flags().BoolVar(&exposeWait, "wait", false, "wait until something is listening on the guest port")
	rootCmd.AddCommand(exposeCmd)
}

func runExpose(cmd *cobra.Command, args []string) error {
	if instanceFlag {
		return fmt.Errorf("--instance is not used by expose; specify instances in SOURCE and --as")
	}

	src, err := parseExposeEndpoint(args[0], false)
	if err != nil {
		return err
	}

	dst, err := parseExposeDestination(exposeAs)
	if err != nil {
		return err
	}

	srcPort, dstPort, hostMode, err := resolveExposePorts(src, dst)
	if err != nil {
		return err
	}

	if hostMode {
		resp, err := exposeHostPort(src.Instance, srcPort, dstPort, true)
		if err != nil {
			return err
		}
		if exposeWait {
			if err := waitForPort(resp.HostPort, 30*time.Second); err != nil {
				return fmt.Errorf("waiting for %s:%d: %w", src.Instance, srcPort, err)
			}
		}
		fmt.Printf("localhost:%d -> %s:%d\n", resp.HostPort, src.Instance, srcPort)
		return nil
	}

	if src.Instance == dst.Instance && srcPort == dstPort {
		return fmt.Errorf("%s:%d cannot be exposed onto itself", src.Instance, srcPort)
	}

	resp, err := exposeHostPort(src.Instance, srcPort, 0, false)
	if err != nil {
		return err
	}
	if _, err := exposeGuestPort(dst.Instance, dstPort, guestHostGateway, resp.HostPort); err != nil {
		if resp.Created {
			if rollbackErr := removeHostExpose(src.Instance, resp.HostPort); rollbackErr != nil {
				return fmt.Errorf("%w (rollback failed: %v)", err, rollbackErr)
			}
		}
		return err
	}

	fmt.Printf("%s:%d -> %s:%d\n", dst.Instance, dstPort, src.Instance, srcPort)
	return nil
}

func parseExposeDestination(s string) (exposeEndpoint, error) {
	if s == "" {
		return exposeEndpoint{}, nil
	}
	return parseExposeEndpoint(s, true)
}

func parseExposeEndpoint(s string, allowEmptyInstance bool) (exposeEndpoint, error) {
	if s == "" {
		return exposeEndpoint{}, fmt.Errorf("endpoint is required")
	}

	parts := strings.SplitN(s, ":", 2)
	inst := parts[0]
	if inst == "" && !allowEmptyInstance {
		return exposeEndpoint{}, fmt.Errorf("instance name is required")
	}

	ep := exposeEndpoint{Instance: inst}
	if len(parts) == 1 || parts[1] == "" {
		return ep, nil
	}

	n, err := strconv.ParseUint(parts[1], 10, 16)
	if err != nil || n == 0 {
		return exposeEndpoint{}, fmt.Errorf("invalid port %q", parts[1])
	}
	ep.Port = uint16(n)
	ep.PortSet = true
	return ep, nil
}

func resolveExposePorts(src, dst exposeEndpoint) (srcPort, dstPort uint16, hostMode bool, err error) {
	hostMode = dst.Instance == ""

	switch {
	case src.PortSet:
		srcPort = src.Port
	case dst.PortSet:
		srcPort = dst.Port
	}

	switch {
	case hostMode && dst.PortSet:
		dstPort = dst.Port
	case hostMode:
		dstPort = srcPort
	case dst.PortSet:
		dstPort = dst.Port
	default:
		dstPort = srcPort
	}

	if srcPort == 0 || dstPort == 0 {
		return 0, 0, false, fmt.Errorf("a port must be specified on SOURCE or --as")
	}
	return srcPort, dstPort, hostMode, nil
}

func exposeHostPort(instance string, guestPort, hostPort uint16, visible bool) (*lnx.ExposeHostResponse, error) {
	req := lnx.ExposeHostRequest{
		GuestPort: guestPort,
		HostPort:  hostPort,
		Visible:   visible,
	}
	var resp lnx.ExposeHostResponse
	if err := postInstanceJSON(instance, "/expose/host", req, &resp); err != nil {
		return nil, fmt.Errorf("expose %s:%d on host: %w", instance, guestPort, err)
	}
	return &resp, nil
}

func exposeGuestPort(instance string, listenPort uint16, host string, hostPort uint16) (*lnx.GuestExposeResponse, error) {
	req := lnx.GuestExposeRequest{
		ListenPort: listenPort,
		Host:       host,
		HostPort:   hostPort,
	}
	var resp lnx.GuestExposeResponse
	if err := postInstanceJSON(instance, "/guest/expose", req, &resp); err != nil {
		return nil, fmt.Errorf("expose %s:%d: %w", instance, listenPort, err)
	}
	return &resp, nil
}

func removeHostExpose(instance string, hostPort uint16) error {
	req := lnx.RemoveExposeHostRequest{HostPort: hostPort}
	if err := postInstanceJSON(instance, "/expose/host/remove", req, nil); err != nil {
		return fmt.Errorf("remove host expose %s:%d: %w", instance, hostPort, err)
	}
	return nil
}

// waitForPort probes localhost:port until the guest is actually serving.
// A bare TCP connect isn't enough — the host port forward listener accepts
// immediately, but the forwarded connection resets if nothing is listening
// in the guest. We connect, then read without sending: if the read blocks
// (times out), the guest accepted the connection and is waiting for data,
// meaning the service is up. If the read returns immediately with an error
// (reset/EOF), the guest isn't ready yet and we retry.
func waitForPort(port uint16, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	addr := fmt.Sprintf("127.0.0.1:%d", port)
	for time.Now().Before(deadline) {
		conn, err := net.DialTimeout("tcp", addr, time.Second)
		if err != nil {
			time.Sleep(200 * time.Millisecond)
			continue
		}
		conn.SetReadDeadline(time.Now().Add(500 * time.Millisecond))
		buf := make([]byte, 1)
		_, err = conn.Read(buf)
		conn.Close()
		if err != nil && isTimeout(err) {
			// Read timed out — the guest accepted and is waiting for
			// data, so the service is up.
			return nil
		}
		// Connection closed or reset immediately — guest not ready.
		time.Sleep(200 * time.Millisecond)
	}
	return fmt.Errorf("timed out after %s", timeout)
}

func isTimeout(err error) bool {
	if ne, ok := err.(net.Error); ok {
		return ne.Timeout()
	}
	return false
}

func postInstanceJSON(instance, path string, reqBody any, respBody any) error {
	data, err := json.Marshal(reqBody)
	if err != nil {
		return err
	}

	resp, err := apiClientFor(instance).Post("http://localhost"+path, "application/json", bytes.NewReader(data))
	if err != nil {
		if isNoVM(err) {
			return fmt.Errorf("no VM running for instance %q", instance)
		}
		return err
	}
	defer resp.Body.Close()

	if resp.StatusCode/100 != 2 {
		var msg bytes.Buffer
		_, _ = msg.ReadFrom(resp.Body)
		text := strings.TrimSpace(msg.String())
		if text == "" {
			text = resp.Status
		}
		if resp.StatusCode == http.StatusConflict {
			return fmt.Errorf("%s", text)
		}
		return fmt.Errorf("%s", text)
	}

	if respBody != nil {
		if err := json.NewDecoder(resp.Body).Decode(respBody); err != nil {
			return err
		}
	}
	return nil
}
