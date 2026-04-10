package lnx_test

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

func TestBuildInLnxAutoProvisionsMissingBuildInstance(t *testing.T) {
	tmp := t.TempDir()
	logPath := filepath.Join(tmp, "calls.log")
	fakeLnx := filepath.Join(tmp, "fake-lnx")
	script := `#!/bin/sh
echo "$*" >> "$LOG_FILE"
if [ "$1" = "--instance" ] && [ "$2" = "build" ] && [ "$3" = "true" ]; then
  exit 1
fi
if [ "$1" = "true" ]; then
  exit 0
fi
if [ "$1" = "clone" ] && [ "$2" = "build" ]; then
  exit 0
fi
exit 0
`
	if err := os.WriteFile(fakeLnx, []byte(script), 0755); err != nil {
		t.Fatalf("write fake lnx: %v", err)
	}

	cmd := exec.Command("sh", "./scripts/build-in-lnx.sh", "kernel")
	cmd.Dir = "/Users/ramon/src/lnx"
	cmd.Env = append(os.Environ(),
		"LNX_BIN="+fakeLnx,
		"LNX_BUILD_INSTANCE=build",
		"LOG_FILE="+logPath,
	)
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("build-in-lnx.sh failed: %v\n%s", err, out)
	}

	data, err := os.ReadFile(logPath)
	if err != nil {
		t.Fatalf("read log: %v", err)
	}
	lines := strings.Split(strings.TrimSpace(string(data)), "\n")
	if len(lines) < 4 {
		t.Fatalf("expected at least 4 lnx invocations, got %v", lines)
	}
	if lines[0] != "--instance build true" {
		t.Fatalf("first invocation = %q", lines[0])
	}
	if lines[1] != "true" {
		t.Fatalf("second invocation = %q", lines[1])
	}
	if lines[2] != "clone build" {
		t.Fatalf("third invocation = %q", lines[2])
	}
	if !strings.HasPrefix(lines[3], "--instance build sh -lc ") {
		t.Fatalf("fourth invocation = %q", lines[3])
	}
}
