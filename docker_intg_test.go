//go:build darwin && integration

package lnx_test

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/semistrict/lnx"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

// waitForDocker is a shell snippet that starts Docker and waits for it to be ready.
const waitForDocker = `sudo systemctl start docker && for i in $(seq 1 20); do docker info >/dev/null 2>&1 && break; sleep 1; done`

func TestDocker_HelloWorld(t *testing.T) {
	t.Parallel()
	dir := setupTestDir(t)
	cfg := testConfig(dir)

	// Start Docker, pull and run hello-world.
	exitCode, err := lnx.Run(cfg, "sh", "-c",
		waitForDocker + ` &&
		docker run --rm hello-world 2>&1 | grep -q "Hello from Docker"`)
	require.NoError(t, err)
	if exitCode == 127 {
		t.Skip("skipping: Docker not installed in rootfs")
	}
	assert.Equal(t, 0, exitCode)
}

func TestDocker_BuildAndRun(t *testing.T) {
	t.Parallel()
	dir := setupTestDir(t)
	cfg := testConfig(dir)

	// Create a Dockerfile in the CWD, build an image, run it.
	buildDir := t.TempDir()
	require.NoError(t, os.WriteFile(filepath.Join(buildDir, "Dockerfile"), []byte(`
FROM alpine:latest
RUN echo "built-ok" > /msg.txt
CMD cat /msg.txt
`), 0644))

	cfg.CWD = buildDir
	exitCode, err := lnx.Run(cfg, "sh", "-c",
		waitForDocker + ` &&
		docker build -t lnx-test-build . &&
		docker run --rm lnx-test-build | grep -q "built-ok"`)
	require.NoError(t, err)
	if exitCode == 127 {
		t.Skip("skipping: Docker not installed in rootfs")
	}
	assert.Equal(t, 0, exitCode)
}

func TestDocker_ComposeUpDown(t *testing.T) {
	t.Parallel()
	dir := setupTestDir(t)
	cfg := testConfig(dir)

	// Create a docker-compose.yml with two services that communicate.
	composeDir := t.TempDir()
	require.NoError(t, os.WriteFile(filepath.Join(composeDir, "compose.yaml"), []byte(`
services:
  web:
    image: alpine:latest
    command: ["sh", "-c", "echo COMPOSE_OK > /tmp/result.txt && cat /tmp/result.txt"]
`), 0644))

	cfg.CWD = composeDir
	exitCode, err := lnx.Run(cfg, "sh", "-c",
		waitForDocker + ` &&
		docker compose up --exit-code-from web 2>&1 | grep -q "COMPOSE_OK"`)
	require.NoError(t, err)
	if exitCode == 127 {
		t.Skip("skipping: Docker not installed in rootfs")
	}
	assert.Equal(t, 0, exitCode)
}

func TestDocker_Networking(t *testing.T) {
	t.Parallel()
	dir := setupTestDir(t)
	cfg := testConfig(dir)

	// Run an nginx container and curl it from another container on the same network.
	exitCode, err := lnx.Run(cfg, "sh", "-c",
		waitForDocker + ` &&
		docker network create lnx-test-net &&
		docker run -d --name lnx-nginx --network lnx-test-net nginx:alpine &&
		sleep 3 &&
		docker run --rm --network lnx-test-net alpine:latest sh -c "apk add --no-cache curl >/dev/null 2>&1 && curl -sf http://lnx-nginx/" | grep -q "Welcome to nginx" &&
		docker rm -f lnx-nginx &&
		docker network rm lnx-test-net`)
	require.NoError(t, err)
	if exitCode == 127 {
		t.Skip("skipping: Docker not installed in rootfs")
	}
	assert.Equal(t, 0, exitCode)
}
