package main

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/spf13/cobra"
	"golang.org/x/net/dns/dnsmessage"
)

const (
	defaultIngressDomain   = "lnx"
	defaultIngressDNSAddr  = "127.0.0.1:5354"
	defaultIngressHTTPAddr = "127.0.0.1:80"
)

var (
	ingressSpawn   bool
	ingressCleanup bool
)

var ingressCmd = &cobra.Command{
	Use:   "ingress",
	Short: "Manage local .lnx HTTP ingress",
}

var ingressEnableCmd = &cobra.Command{
	Use:   "enable",
	Short: "Enable local .lnx DNS and HTTP ingress",
	RunE:  runIngressEnable,
}

var ingressDisableCmd = &cobra.Command{
	Use:   "disable",
	Short: "Disable local .lnx DNS and HTTP ingress",
	RunE:  runIngressDisable,
}

var ingressStatusCmd = &cobra.Command{
	Use:   "status",
	Short: "Show local .lnx ingress status",
	RunE:  runIngressStatus,
}

var ingressHiddenCmd = &cobra.Command{
	Use:    "_ingress",
	Short:  "Run local ingress helper (internal use)",
	Hidden: true,
	RunE: func(cmd *cobra.Command, args []string) error {
		if runtime.GOOS != "darwin" {
			return fmt.Errorf("ingress is only supported on macOS")
		}
		cfg := loadIngressConfig()
		switch {
		case ingressCleanup:
			return runIngressCleanup(cfg)
		case ingressSpawn:
			return spawnIngressDaemon(cfg)
		default:
			return runIngressDaemon(cfg)
		}
	},
}

type ingressConfig struct {
	Domain      string
	DNSAddr     string
	HTTPAddr    string
	ResolverDir string
	StateDir    string
}

type ingressStatus struct {
	Enabled      bool   `json:"enabled"`
	Domain       string `json:"domain"`
	DNSAddr      string `json:"dns_addr"`
	HTTPAddr     string `json:"http_addr"`
	ResolverPath string `json:"resolver_path"`
	PID          int    `json:"pid"`
}

type ingressRoute struct {
	Instance string
	Port     uint16
}

func init() {
	ingressCmd.AddCommand(ingressEnableCmd, ingressDisableCmd, ingressStatusCmd)
	ingressHiddenCmd.Flags().BoolVar(&ingressSpawn, "spawn", false, "spawn ingress daemon and exit")
	ingressHiddenCmd.Flags().BoolVar(&ingressCleanup, "cleanup", false, "remove ingress resolver and stale socket")
	rootCmd.AddCommand(ingressCmd)
	rootCmd.AddCommand(ingressHiddenCmd)
}

func loadIngressConfig() ingressConfig {
	return ingressConfig{
		Domain:      envOr("LNX_INGRESS_DOMAIN", defaultIngressDomain),
		DNSAddr:     envOr("LNX_INGRESS_DNS_ADDR", defaultIngressDNSAddr),
		HTTPAddr:    envOr("LNX_INGRESS_HTTP_ADDR", defaultIngressHTTPAddr),
		ResolverDir: envOr("LNX_INGRESS_RESOLVER_DIR", "/etc/resolver"),
		StateDir:    envOr("LNX_INGRESS_STATE_DIR", filepath.Join(lnxBase(), "ingress")),
	}
}

func envOr(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
}

func (cfg ingressConfig) socketPath() string {
	return filepath.Join(cfg.StateDir, "ingress.sock")
}

func (cfg ingressConfig) logPath() string {
	return filepath.Join(cfg.StateDir, "ingress.log")
}

func (cfg ingressConfig) resolverPath() string {
	return filepath.Join(cfg.ResolverDir, cfg.Domain)
}

func (cfg ingressConfig) resolverContents() (string, error) {
	host, port, err := net.SplitHostPort(cfg.DNSAddr)
	if err != nil {
		return "", fmt.Errorf("parse dns addr %q: %w", cfg.DNSAddr, err)
	}
	if host == "" {
		host = "127.0.0.1"
	}
	return fmt.Sprintf("nameserver %s\nport %s\n", host, port), nil
}

func (cfg ingressConfig) needsPrivileges() bool {
	if runtime.GOOS != "darwin" || os.Getuid() == 0 {
		return false
	}
	if requiresPrivilegedPort(cfg.HTTPAddr) || requiresPrivilegedPort(cfg.DNSAddr) {
		return true
	}
	return filepath.Clean(cfg.ResolverDir) == "/etc/resolver"
}

func requiresPrivilegedPort(addr string) bool {
	_, port, err := net.SplitHostPort(addr)
	if err != nil {
		return true
	}
	n, err := strconv.Atoi(port)
	if err != nil {
		return true
	}
	return n > 0 && n < 1024
}

func runIngressEnable(cmd *cobra.Command, args []string) error {
	if runtime.GOOS != "darwin" {
		return fmt.Errorf("ingress is only supported on macOS")
	}

	cfg := loadIngressConfig()
	if status, err := fetchIngressStatus(cfg); err == nil && status.Enabled {
		fmt.Printf("ingress enabled for .%s\n", status.Domain)
		return nil
	}

	if err := startIngressHelper(cfg); err != nil {
		return err
	}
	status, err := waitForIngressStatus(cfg, 10*time.Second)
	if err != nil {
		if logData, readErr := os.ReadFile(cfg.logPath()); readErr == nil {
			return fmt.Errorf("wait for ingress: %w\n%s", err, strings.TrimSpace(string(logData)))
		}
		return fmt.Errorf("wait for ingress: %w", err)
	}

	fmt.Printf("ingress enabled for .%s\n", status.Domain)
	return nil
}

func runIngressDisable(cmd *cobra.Command, args []string) error {
	if runtime.GOOS != "darwin" {
		return fmt.Errorf("ingress is only supported on macOS")
	}

	cfg := loadIngressConfig()
	stopped := false
	if err := stopIngress(cfg); err == nil {
		stopped = true
	} else if !isNoIngress(err) {
		return err
	}

	if stopped {
		if err := waitForIngressStop(cfg, 5*time.Second); err != nil {
			return err
		}
	}

	if pathExists(cfg.socketPath()) || pathExists(cfg.resolverPath()) {
		if err := cleanupIngressHelper(cfg); err != nil {
			return err
		}
	}

	if stopped || pathExists(cfg.resolverPath()) {
		fmt.Println("ingress disabled")
		return nil
	}
	fmt.Println("ingress already disabled")
	return nil
}

func runIngressStatus(cmd *cobra.Command, args []string) error {
	if runtime.GOOS != "darwin" {
		return fmt.Errorf("ingress is only supported on macOS")
	}

	cfg := loadIngressConfig()
	status, err := fetchIngressStatus(cfg)
	if err != nil {
		if isNoIngress(err) {
			fmt.Println("disabled")
			return nil
		}
		return err
	}

	fmt.Println("enabled")
	fmt.Printf("domain: .%s\n", status.Domain)
	fmt.Printf("dns: %s\n", status.DNSAddr)
	fmt.Printf("http: %s\n", status.HTTPAddr)
	fmt.Printf("resolver: %s\n", status.ResolverPath)
	return nil
}

func startIngressHelper(cfg ingressConfig) error {
	self, err := os.Executable()
	if err != nil {
		return fmt.Errorf("find executable: %w", err)
	}
	cmd := buildIngressHelperCmd(self, []string{"_ingress", "--spawn"}, cfg)
	cmd.Stdin = os.Stdin
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("start ingress helper %v: %w", cmd.Args, err)
	}
	return nil
}

func cleanupIngressHelper(cfg ingressConfig) error {
	self, err := os.Executable()
	if err != nil {
		return fmt.Errorf("find executable: %w", err)
	}
	cmd := buildIngressHelperCmd(self, []string{"_ingress", "--cleanup"}, cfg)
	cmd.Stdin = os.Stdin
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("cleanup ingress helper %v: %w", cmd.Args, err)
	}
	return nil
}

func buildIngressHelperCmd(self string, args []string, cfg ingressConfig) *exec.Cmd {
	if cfg.needsPrivileges() {
		envArgs := []string{fmt.Sprintf("HOME=%s", os.Getenv("HOME"))}
		for _, key := range []string{
			"LNX_LOG",
			"LNX_INGRESS_DOMAIN",
			"LNX_INGRESS_DNS_ADDR",
			"LNX_INGRESS_HTTP_ADDR",
			"LNX_INGRESS_RESOLVER_DIR",
			"LNX_INGRESS_STATE_DIR",
		} {
			if v := os.Getenv(key); v != "" {
				envArgs = append(envArgs, fmt.Sprintf("%s=%s", key, v))
			}
		}
		sudoArgs := append(envArgs, self)
		sudoArgs = append(sudoArgs, args...)
		return exec.Command("sudo", sudoArgs...)
	}
	cmd := exec.Command(self, args...)
	cmd.Env = os.Environ()
	return cmd
}

func spawnIngressDaemon(cfg ingressConfig) error {
	if err := os.MkdirAll(cfg.StateDir, 0755); err != nil {
		return fmt.Errorf("create ingress state dir: %w", err)
	}
	self, err := os.Executable()
	if err != nil {
		return fmt.Errorf("find executable: %w", err)
	}
	logFile, err := os.OpenFile(cfg.logPath(), os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0644)
	if err != nil {
		return fmt.Errorf("open ingress log: %w", err)
	}

	cmd := exec.Command(self, "_ingress")
	cmd.Env = os.Environ()
	cmd.Stdin = nil
	cmd.Stdout = logFile
	cmd.Stderr = logFile
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true}

	if err := cmd.Start(); err != nil {
		logFile.Close()
		return fmt.Errorf("spawn ingress daemon: %w", err)
	}
	_ = cmd.Process.Release()
	return logFile.Close()
}

func runIngressDaemon(cfg ingressConfig) error {
	initIngressLogging(cfg.logPath())

	if err := os.MkdirAll(cfg.StateDir, 0755); err != nil {
		return fmt.Errorf("create ingress state dir: %w", err)
	}

	httpLn, err := net.Listen("tcp", cfg.HTTPAddr)
	if err != nil {
		return fmt.Errorf("listen http %s: %w", cfg.HTTPAddr, err)
	}
	defer httpLn.Close()

	dnsConn, err := net.ListenPacket("udp", cfg.DNSAddr)
	if err != nil {
		return fmt.Errorf("listen dns %s: %w", cfg.DNSAddr, err)
	}
	defer dnsConn.Close()

	if err := installIngressResolver(cfg); err != nil {
		return err
	}
	defer func() {
		if err := removeIngressResolver(cfg); err != nil && !os.IsNotExist(err) {
			slog.Warn("remove ingress resolver failed", "error", err)
		}
	}()

	stopCh := make(chan struct{})
	stopOnce := sync.Once{}
	stop := func() { stopOnce.Do(func() { close(stopCh) }) }

	adminLn, err := listenIngressAdmin(cfg.socketPath())
	if err != nil {
		return err
	}
	defer func() {
		adminLn.Close()
		_ = os.Remove(cfg.socketPath())
	}()

	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()

	proxy := newIngressProxy(cfg)
	httpSrv := &http.Server{Handler: proxy}
	adminSrv := &http.Server{
		Handler: ingressAdminMux(cfg, stop),
	}

	go serveIngressDNS(dnsConn, cfg.Domain, stopCh)
	go func() {
		if err := httpSrv.Serve(httpLn); err != nil && err != http.ErrServerClosed {
			slog.Error("ingress http server failed", "error", err)
			stop()
		}
	}()
	go func() {
		if err := adminSrv.Serve(adminLn); err != nil && err != http.ErrServerClosed {
			slog.Error("ingress admin server failed", "error", err)
			stop()
		}
	}()

	select {
	case <-ctx.Done():
	case <-stopCh:
	}

	shutdownCtx, shutdownCancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer shutdownCancel()
	_ = adminSrv.Shutdown(shutdownCtx)
	_ = httpSrv.Shutdown(shutdownCtx)
	return nil
}

func runIngressCleanup(cfg ingressConfig) error {
	if err := removeIngressResolver(cfg); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("remove ingress resolver: %w", err)
	}
	if err := os.Remove(cfg.socketPath()); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("remove ingress socket: %w", err)
	}
	return nil
}

func installIngressResolver(cfg ingressConfig) error {
	if err := os.MkdirAll(cfg.ResolverDir, 0755); err != nil {
		return fmt.Errorf("create resolver dir %s: %w", cfg.ResolverDir, err)
	}
	contents, err := cfg.resolverContents()
	if err != nil {
		return err
	}
	if err := os.WriteFile(cfg.resolverPath(), []byte(contents), 0644); err != nil {
		return fmt.Errorf("write resolver %s: %w", cfg.resolverPath(), err)
	}
	return nil
}

func removeIngressResolver(cfg ingressConfig) error {
	return os.Remove(cfg.resolverPath())
}

func listenIngressAdmin(sockPath string) (net.Listener, error) {
	if err := os.MkdirAll(filepath.Dir(sockPath), 0755); err != nil {
		return nil, fmt.Errorf("create ingress socket dir: %w", err)
	}
	_ = os.Remove(sockPath)
	ln, err := net.Listen("unix", sockPath)
	if err != nil {
		return nil, fmt.Errorf("listen ingress socket %s: %w", sockPath, err)
	}
	if err := os.Chmod(sockPath, 0666); err != nil {
		ln.Close()
		return nil, fmt.Errorf("chmod ingress socket %s: %w", sockPath, err)
	}
	return ln, nil
}

func ingressAdminMux(cfg ingressConfig, stop func()) http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /status", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(ingressStatus{
			Enabled:      true,
			Domain:       cfg.Domain,
			DNSAddr:      cfg.DNSAddr,
			HTTPAddr:     cfg.HTTPAddr,
			ResolverPath: cfg.resolverPath(),
			PID:          os.Getpid(),
		})
	})
	mux.HandleFunc("POST /stop", func(w http.ResponseWriter, r *http.Request) {
		stop()
		w.WriteHeader(http.StatusNoContent)
	})
	return mux
}

type ingressProxy struct {
	cfg ingressConfig
}

func newIngressProxy(cfg ingressConfig) *ingressProxy {
	return &ingressProxy{cfg: cfg}
}

func (p *ingressProxy) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	route, err := parseIngressHost(r.Host, p.cfg.Domain)
	if err != nil {
		http.NotFound(w, r)
		return
	}

	resp, err := exposeHostPort(route.Instance, route.Port, 0, false)
	if err != nil {
		http.Error(w, err.Error(), http.StatusBadGateway)
		return
	}

	target := &url.URL{
		Scheme: "http",
		Host:   net.JoinHostPort("127.0.0.1", strconv.Itoa(int(resp.HostPort))),
	}
	backend := &httputil.ReverseProxy{
		Rewrite: func(pr *httputil.ProxyRequest) {
			pr.SetURL(target)
			pr.Out.Host = pr.In.Host
		},
		ErrorHandler: func(w http.ResponseWriter, r *http.Request, err error) {
			http.Error(w, err.Error(), http.StatusBadGateway)
		},
	}
	backend.ServeHTTP(w, r)
}

func parseIngressHost(host, domain string) (ingressRoute, error) {
	host = stripOptionalPort(host)
	host = strings.TrimSuffix(strings.ToLower(host), ".")
	suffix := "." + strings.ToLower(domain)
	if !strings.HasSuffix(host, suffix) {
		return ingressRoute{}, fmt.Errorf("host %q is not under .%s", host, domain)
	}
	name := strings.TrimSuffix(host, suffix)
	labels := strings.Split(name, ".")
	if len(labels) < 2 {
		return ingressRoute{}, fmt.Errorf("host %q must look like p<port>.<instance>.%s", host, domain)
	}

	portLabel := labels[0]
	if !strings.HasPrefix(portLabel, "p") || len(portLabel) == 1 {
		return ingressRoute{}, fmt.Errorf("host %q must start with p<port>", host)
	}
	n, err := strconv.ParseUint(portLabel[1:], 10, 16)
	if err != nil || n == 0 {
		return ingressRoute{}, fmt.Errorf("invalid ingress port %q", portLabel)
	}

	instance := strings.Join(labels[1:], ".")
	if instance == "" {
		return ingressRoute{}, fmt.Errorf("missing instance in host %q", host)
	}
	return ingressRoute{Instance: instance, Port: uint16(n)}, nil
}

func stripOptionalPort(host string) string {
	if h, _, err := net.SplitHostPort(host); err == nil {
		return h
	}
	return host
}

func serveIngressDNS(conn net.PacketConn, domain string, stopCh <-chan struct{}) {
	buf := make([]byte, 1500)
	for {
		_ = conn.SetReadDeadline(time.Now().Add(500 * time.Millisecond))
		n, addr, err := conn.ReadFrom(buf)
		if err != nil {
			if ne, ok := err.(net.Error); ok && ne.Timeout() {
				select {
				case <-stopCh:
					return
				default:
					continue
				}
			}
			return
		}
		resp, err := ingressDNSResponse(buf[:n], domain)
		if err != nil {
			continue
		}
		_, _ = conn.WriteTo(resp, addr)
	}
}

func ingressDNSResponse(packet []byte, domain string) ([]byte, error) {
	var msg dnsmessage.Message
	if err := msg.Unpack(packet); err != nil {
		return nil, err
	}

	resp := dnsmessage.Message{
		Header: dnsmessage.Header{
			ID:                 msg.Header.ID,
			Response:           true,
			Authoritative:      true,
			RecursionDesired:   msg.Header.RecursionDesired,
			RecursionAvailable: false,
		},
		Questions: msg.Questions,
	}

	for _, q := range msg.Questions {
		name := strings.TrimSuffix(q.Name.String(), ".")
		if _, err := parseIngressHost(name, domain); err != nil {
			resp.Header.RCode = dnsmessage.RCodeNameError
			resp.Answers = nil
			break
		}
		if q.Class != dnsmessage.ClassINET {
			continue
		}
		if q.Type != dnsmessage.TypeA {
			continue
		}
		resp.Answers = append(resp.Answers, dnsmessage.Resource{
			Header: dnsmessage.ResourceHeader{
				Name:  q.Name,
				Type:  dnsmessage.TypeA,
				Class: dnsmessage.ClassINET,
				TTL:   1,
			},
			Body: &dnsmessage.AResource{A: [4]byte{127, 0, 0, 1}},
		})
	}

	return resp.Pack()
}

func ingressClient(cfg ingressConfig) *http.Client {
	return &http.Client{
		Transport: &http.Transport{
			DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
				conn, err := net.DialTimeout("unix", cfg.socketPath(), 500*time.Millisecond)
				if err != nil {
					return nil, fmt.Errorf("no ingress socket at %s", cfg.socketPath())
				}
				return conn, nil
			},
		},
	}
}

func fetchIngressStatus(cfg ingressConfig) (*ingressStatus, error) {
	resp, err := ingressClient(cfg).Get("http://localhost/status")
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	var status ingressStatus
	if err := json.NewDecoder(resp.Body).Decode(&status); err != nil {
		return nil, fmt.Errorf("read ingress status: %w", err)
	}
	return &status, nil
}

func stopIngress(cfg ingressConfig) error {
	resp, err := ingressClient(cfg).Post("http://localhost/stop", "", nil)
	if err != nil {
		return err
	}
	resp.Body.Close()
	return nil
}

func waitForIngressStatus(cfg ingressConfig, timeout time.Duration) (*ingressStatus, error) {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		status, err := fetchIngressStatus(cfg)
		if err == nil {
			return status, nil
		}
		time.Sleep(100 * time.Millisecond)
	}
	return nil, fmt.Errorf("timed out after %s", timeout)
}

func waitForIngressStop(cfg ingressConfig, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		if _, err := fetchIngressStatus(cfg); isNoIngress(err) {
			return nil
		}
		time.Sleep(100 * time.Millisecond)
	}
	return fmt.Errorf("timed out waiting for ingress to stop")
}

func isNoIngress(err error) bool {
	if err == nil {
		return false
	}
	s := err.Error()
	return strings.Contains(s, "no ingress socket") ||
		strings.Contains(s, "no such file") ||
		strings.Contains(s, "connection refused")
}

func initIngressLogging(path string) {
	if err := os.MkdirAll(filepath.Dir(path), 0755); err != nil {
		return
	}
	f, err := os.OpenFile(path, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0644)
	if err != nil {
		return
	}
	slog.SetDefault(slog.New(slog.NewTextHandler(f, &slog.HandlerOptions{Level: slog.LevelInfo})))
}

func pathExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}
