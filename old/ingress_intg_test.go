//go:build darwin && integration

package lnx_test

import (
	"bytes"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"golang.org/x/net/dns/dnsmessage"
)

func TestCLI_Ingress_EnableStatusAndProxy(t *testing.T) {
	bin := lnxBin()
	if bin == "" {
		t.Skip("lnx not in PATH")
	}

	srcInst := fmt.Sprintf("test-ingress-src-%d", time.Now().UnixNano())
	createClonedInstance(t, srcInst)
	registerInstanceStopCleanup(t, bin, srcInst)

	stateDir, err := os.MkdirTemp("/tmp", "lnx-ingress-state-*")
	require.NoError(t, err)
	t.Cleanup(func() { _ = os.RemoveAll(stateDir) })
	resolverDir := filepath.Join(t.TempDir(), "resolver")
	httpAddr := "127.0.0.1:18080"
	dnsAddr := "127.0.0.1:15354"
	env := append(os.Environ(),
		"LNX_INGRESS_STATE_DIR="+stateDir,
		"LNX_INGRESS_RESOLVER_DIR="+resolverDir,
		"LNX_INGRESS_HTTP_ADDR="+httpAddr,
		"LNX_INGRESS_DNS_ADDR="+dnsAddr,
	)

	t.Cleanup(func() {
		_, _ = runCLIEnv(bin, env, "ingress", "disable")
	})

	guestPort := 18380
	hostName := fmt.Sprintf("p%d.%s.lnx", guestPort, srcInst)
	payload := "HELLO_INGRESS"

	srcCmd, srcLines, srcStderr, srcDone := startHTTPServerInstance(t, bin, srcInst, guestPort, payload)
	waitForCLIOutput(t, srcLines, "READY", 20*time.Second, srcStderr)
	t.Cleanup(func() { cleanupStreamingCLI(t, srcCmd, srcDone, srcStderr) })

	enableOut := runCLISuccessEnv(t, bin, env, "ingress", "enable")
	assert.Contains(t, enableOut, "writing "+filepath.Join(resolverDir, "lnx"))
	assert.Contains(t, enableOut, "starting dns on 127.0.0.1:15354")
	assert.Contains(t, enableOut, "starting http on 127.0.0.1:18080")
	assert.Contains(t, enableOut, "ingress enabled for .lnx")

	resolverPath := filepath.Join(resolverDir, "lnx")
	resolverData, err := os.ReadFile(resolverPath)
	require.NoError(t, err)
	assert.Contains(t, string(resolverData), "nameserver 127.0.0.1")
	assert.Contains(t, string(resolverData), "port 15354")

	statusOut := runCLISuccessEnv(t, bin, env, "ingress", "status")
	assert.Contains(t, statusOut, "enabled")
	assert.Contains(t, statusOut, "dns: 127.0.0.1:15354")
	assert.Contains(t, statusOut, "http: 127.0.0.1:18080")

	ip := lookupIngressA(t, dnsAddr, hostName)
	assert.Equal(t, "127.0.0.1", ip)

	body := httpGetEventually(t, "http://"+httpAddr+"/", hostName, payload, 10*time.Second)
	assert.Contains(t, body, payload)

	disableOut := runCLISuccessEnv(t, bin, env, "ingress", "disable")
	assert.Contains(t, disableOut, "removing "+filepath.Join(resolverDir, "lnx"))
	assert.Contains(t, disableOut, "ingress disabled")

	statusAfter := runCLISuccessEnv(t, bin, env, "ingress", "status")
	assert.Contains(t, statusAfter, "disabled")
	_, err = os.Stat(resolverPath)
	require.ErrorIs(t, err, os.ErrNotExist)

	waitForProcessSuccess(t, srcDone, 15*time.Second, srcStderr.String())
}

func startHTTPServerInstance(t *testing.T, bin, instance string, port int, payload string) (*exec.Cmd, <-chan string, *bytes.Buffer, <-chan error) {
	t.Helper()
	script := fmt.Sprintf(`python3 -c "
import socket, time
body = b'%s'
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('0.0.0.0', %d))
s.listen(1)
print('READY', flush=True)
conn, _ = s.accept()
data = b''
while b'\\r\\n\\r\\n' not in data:
    chunk = conn.recv(4096)
    if not chunk:
        break
    data += chunk
resp = b'HTTP/1.1 200 OK\\r\\nContent-Type: text/plain\\r\\nContent-Length: ' + str(len(body)).encode() + b'\\r\\nConnection: close\\r\\n\\r\\n' + body
conn.sendall(resp)
conn.close()
s.close()
time.sleep(1)
"`, payload, port)
	return startStreamingCLI(t, bin, "--instance", instance, "sh", "-c", script)
}

func runCLIEnv(bin string, env []string, args ...string) (string, error) {
	cmd := exec.Command(bin, args...)
	cmd.Env = env
	out, err := cmd.CombinedOutput()
	return string(out), err
}

func runCLISuccessEnv(t *testing.T, bin string, env []string, args ...string) string {
	t.Helper()
	out, err := runCLIEnv(bin, env, args...)
	require.NoError(t, err, "command failed: %s %v\n%s", bin, args, out)
	return out
}

func lookupIngressA(t *testing.T, serverAddr, host string) string {
	t.Helper()
	name, err := dnsmessage.NewName(host + ".")
	require.NoError(t, err)

	query := dnsmessage.Message{
		Header: dnsmessage.Header{ID: 1, RecursionDesired: true},
		Questions: []dnsmessage.Question{{
			Name:  name,
			Type:  dnsmessage.TypeA,
			Class: dnsmessage.ClassINET,
		}},
	}
	packet, err := query.Pack()
	require.NoError(t, err)

	conn, err := net.Dial("udp", serverAddr)
	require.NoError(t, err)
	defer conn.Close()
	require.NoError(t, conn.SetDeadline(time.Now().Add(5*time.Second)))
	_, err = conn.Write(packet)
	require.NoError(t, err)

	buf := make([]byte, 1500)
	n, err := conn.Read(buf)
	require.NoError(t, err)

	var resp dnsmessage.Message
	require.NoError(t, resp.Unpack(buf[:n]))
	require.NotEmpty(t, resp.Answers)
	a, ok := resp.Answers[0].Body.(*dnsmessage.AResource)
	require.True(t, ok, "unexpected dns answer body %T", resp.Answers[0].Body)
	return net.IP(a.A[:]).String()
}

func httpGetEventually(t *testing.T, rawURL, host, want string, timeout time.Duration) string {
	t.Helper()

	client := &http.Client{Timeout: 2 * time.Second}
	deadline := time.Now().Add(timeout)
	last := ""
	for time.Now().Before(deadline) {
		req, err := http.NewRequest("GET", rawURL, nil)
		require.NoError(t, err)
		req.Host = host

		resp, err := client.Do(req)
		if err == nil {
			data, readErr := io.ReadAll(resp.Body)
			_ = resp.Body.Close()
			if readErr == nil {
				last = string(data)
				if strings.Contains(last, want) {
					return last
				}
			}
		}
		time.Sleep(250 * time.Millisecond)
	}
	t.Fatalf("timed out waiting for %q from %s via host %s; last response=%q", want, rawURL, host, last)
	return ""
}
