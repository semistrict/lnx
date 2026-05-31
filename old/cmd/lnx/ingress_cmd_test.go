package main

import "testing"

func TestParseIngressHost(t *testing.T) {
	tests := []struct {
		name      string
		host      string
		domain    string
		wantInst  string
		wantPort  uint16
		wantError bool
	}{
		{name: "basic host", host: "p8080.dev.lnx", domain: "lnx", wantInst: "dev", wantPort: 8080},
		{name: "host with request port", host: "p8080.dev.lnx:80", domain: "lnx", wantInst: "dev", wantPort: 8080},
		{name: "nested instance", host: "p3000.parent.child.lnx", domain: "lnx", wantInst: "parent.child", wantPort: 3000},
		{name: "wrong suffix", host: "p8080.dev.local", domain: "lnx", wantError: true},
		{name: "missing instance", host: "p8080.lnx", domain: "lnx", wantError: true},
		{name: "missing p prefix", host: "8080.dev.lnx", domain: "lnx", wantError: true},
		{name: "invalid port", host: "pnope.dev.lnx", domain: "lnx", wantError: true},
		{name: "zero port", host: "p0.dev.lnx", domain: "lnx", wantError: true},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			got, err := parseIngressHost(tc.host, tc.domain)
			if tc.wantError {
				if err == nil {
					t.Fatalf("expected error, got %+v", got)
				}
				return
			}
			if err != nil {
				t.Fatalf("unexpected error: %v", err)
			}
			if got.Instance != tc.wantInst || got.Port != tc.wantPort {
				t.Fatalf("got %+v, want instance=%q port=%d", got, tc.wantInst, tc.wantPort)
			}
		})
	}
}

func TestIngressNeedsPrivileges(t *testing.T) {
	t.Setenv("LNX_INGRESS_HTTP_ADDR", "127.0.0.1:18080")
	t.Setenv("LNX_INGRESS_DNS_ADDR", "127.0.0.1:15354")
	t.Setenv("LNX_INGRESS_RESOLVER_DIR", t.TempDir())
	t.Setenv("LNX_INGRESS_STATE_DIR", t.TempDir())

	cfg := loadIngressConfig()
	if cfg.needsPrivileges() {
		t.Fatalf("expected unprivileged ingress config to avoid sudo")
	}
}
