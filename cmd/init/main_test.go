//go:build linux

package main

import (
	"strings"
	"testing"
)

func TestShouldStartEnabledServices(t *testing.T) {
	t.Setenv("LNX_EXPERIMENTS", "")
	t.Setenv("LNX_TOPLEVEL_MODE", "")
	if !shouldStartEnabledServices() {
		t.Fatal("expected enabled services to start in normal mode")
	}

	t.Setenv("LNX_EXPERIMENTS", "memorysnapshot")
	if shouldStartEnabledServices() {
		t.Fatal("expected enabled services to stay off in memorysnapshot wrapper mode")
	}
}

func TestDefaultBrowserEnvUsesHostHelper(t *testing.T) {
	if hostBrowserOpenHelperPath == "xdg-open" {
		t.Fatal("host browser helper must not be plain xdg-open")
	}
	if !strings.HasPrefix(hostBrowserOpenHelperPath, "/usr/local/bin/") {
		t.Fatalf("hostBrowserOpenHelperPath = %q, want /usr/local/bin helper", hostBrowserOpenHelperPath)
	}
}

func TestXdgOpenShimScriptForwardsToHost(t *testing.T) {
	script := xdgOpenShimScript()
	for _, want := range []string{
		`http://localhost/log?level=$level&identifier=lnx-xdg-open`,
		`--data-binary @-`,
		`log_to_host warn "xdg-open called without a URL"`,
		`log_to_host info "forwarded browser open url=$url"`,
		`curl -sf --unix-socket /var/run/lnx/control.sock`,
		`http://localhost/open`,
	} {
		if !strings.Contains(script, want) {
			t.Fatalf("xdgOpenShimScript() missing %q", want)
		}
	}
}
