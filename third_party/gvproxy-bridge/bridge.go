package main

/*
#include <stdint.h>
*/
import "C"

import (
	"context"
	"flag"
	"fmt"
	"io"
	"net"
	"net/url"
	"os"
	"os/signal"
	"sync"
	"sync/atomic"
	"syscall"

	"github.com/containers/gvisor-tap-vsock/pkg/transport"
	"github.com/containers/gvisor-tap-vsock/pkg/types"
	"github.com/containers/gvisor-tap-vsock/pkg/virtualnetwork"
	log "github.com/sirupsen/logrus"
)

type serverHandle struct {
	cancel  context.CancelFunc
	done    chan struct{}
	closer  io.Closer
	logFile *os.File
	socket  string
}

var (
	nextID    int64
	serversMu sync.Mutex
	servers   = map[int64]*serverHandle{}
	lastErrMu sync.Mutex
	lastErr   string
)

func main() {
	vfkitEndpoint := flag.String("listen-vfkit", "", "vfkit unixgram endpoint")
	logPath := flag.String("log", "", "log file path")
	sshPort := flag.Int("ssh-port", 0, "host SSH forwarding port")
	flag.Parse()

	if *vfkitEndpoint == "" {
		fmt.Fprintln(os.Stderr, "--listen-vfkit is required")
		flag.Usage()
		os.Exit(2)
	}

	id, err := start(*vfkitEndpoint, *logPath, *sshPort)
	if err != nil {
		fmt.Fprintf(os.Stderr, "start gvproxy: %s\n", err)
		os.Exit(1)
	}

	signals := make(chan os.Signal, 1)
	signal.Notify(signals, os.Interrupt, syscall.SIGTERM)
	<-signals
	stop(id)
}

//export lnx_gvproxy_start
func lnx_gvproxy_start(vfkitEndpoint *C.char, logPath *C.char, sshPort C.int) C.longlong {
	endpoint := C.GoString(vfkitEndpoint)
	logFile := C.GoString(logPath)
	id, err := start(endpoint, logFile, int(sshPort))
	if err != nil {
		setLastError(err.Error())
		return -1
	}
	return C.longlong(id)
}

//export lnx_gvproxy_stop
func lnx_gvproxy_stop(rawID C.longlong) {
	stop(int64(rawID))
}

func stop(id int64) {
	serversMu.Lock()
	handle := servers[id]
	delete(servers, id)
	serversMu.Unlock()
	if handle == nil {
		return
	}
	log.SetOutput(io.Discard)
	handle.cancel()
	_ = handle.closer.Close()
	<-handle.done
	if handle.logFile != nil {
		_ = handle.logFile.Close()
	}
	_ = os.Remove(handle.socket)
}

//export lnx_gvproxy_last_error
func lnx_gvproxy_last_error() *C.char {
	lastErrMu.Lock()
	defer lastErrMu.Unlock()
	return C.CString(lastErr)
}

func start(endpoint string, logPath string, sshPort int) (int64, error) {
	parsed, err := url.Parse(endpoint)
	if err != nil {
		return 0, fmt.Errorf("parse vfkit endpoint: %w", err)
	}
	if parsed.Scheme != "unixgram" || parsed.Path == "" {
		return 0, fmt.Errorf("vfkit endpoint must be unixgram:/path, got %q", endpoint)
	}

	var logFile *os.File
	if logPath != "" {
		file, err := os.Create(logPath)
		if err != nil {
			return 0, fmt.Errorf("create gvproxy log: %w", err)
		}
		logFile = file
		log.SetOutput(file)
	} else {
		log.SetOutput(io.Discard)
	}
	log.SetLevel(log.InfoLevel)

	config := defaultConfig(sshPort)
	vn, err := virtualnetwork.New(&config)
	if err != nil {
		closeLogFile(logFile)
		return 0, err
	}
	listener, err := transport.ListenUnixgram(endpoint)
	if err != nil {
		closeLogFile(logFile)
		return 0, err
	}
	ctx, cancel := context.WithCancel(context.Background())
	id := atomic.AddInt64(&nextID, 1)
	handle := &serverHandle{
		cancel:  cancel,
		done:    make(chan struct{}),
		closer:  listener,
		logFile: logFile,
		socket:  parsed.Path,
	}
	serversMu.Lock()
	servers[id] = handle
	serversMu.Unlock()

	go func() {
		defer close(handle.done)
		defer listener.Close()
		for {
			select {
			case <-ctx.Done():
				return
			default:
			}
			conn, err := transport.AcceptVfkit(listener)
			if err != nil {
				select {
				case <-ctx.Done():
					return
				default:
					log.Errorf("vfkit accept error: %s", err)
					continue
				}
			}
			go func(conn net.Conn) {
				if err := vn.AcceptVfkit(ctx, conn); err != nil {
					log.Errorf("vfkit connection error: %s", err)
				}
			}(conn)
		}
	}()

	return id, nil
}

func defaultConfig(sshPort int) types.Configuration {
	const (
		subnet    = "192.168.127.0/24"
		gatewayIP = "192.168.127.1"
		deviceIP  = "192.168.127.2"
		hostIP    = "192.168.127.254"
	)
	forwards := map[string]string{}
	if sshPort >= 1024 && sshPort <= 65535 {
		forwards[fmt.Sprintf("127.0.0.1:%d", sshPort)] = net.JoinHostPort(deviceIP, "22")
	}
	return types.Configuration{
		MTU:               1500,
		Subnet:            subnet,
		GatewayIP:         gatewayIP,
		DeviceIP:          deviceIP,
		HostIP:            hostIP,
		GatewayMacAddress: "5a:94:ef:e4:0c:dd",
		Protocol:          types.VfkitProtocol,
		NAT: map[string]string{
			hostIP: "127.0.0.1",
		},
		GatewayVirtualIPs: []string{hostIP},
		Forwards:          forwards,
		DNS: []types.Zone{
			{
				Name: "containers.internal.",
				Records: []types.Record{
					{Name: "gateway", IP: net.ParseIP(gatewayIP)},
					{Name: "host", IP: net.ParseIP(hostIP)},
				},
			},
			{
				Name: "docker.internal.",
				Records: []types.Record{
					{Name: "gateway", IP: net.ParseIP(gatewayIP)},
					{Name: "host", IP: net.ParseIP(hostIP)},
				},
			},
		},
		DHCPStaticLeases: map[string]string{
			deviceIP: "5a:94:ef:e4:0c:ee",
		},
		VpnKitUUIDMacAddresses: map[string]string{
			"c3d68012-0208-11ea-9fd7-f2189899ab08": "5a:94:ef:e4:0c:ee",
		},
	}
}

func setLastError(message string) {
	lastErrMu.Lock()
	defer lastErrMu.Unlock()
	lastErr = message
}

func closeLogFile(file *os.File) {
	if file == nil {
		return
	}
	log.SetOutput(io.Discard)
	_ = file.Close()
}
