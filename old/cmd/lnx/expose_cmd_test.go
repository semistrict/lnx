package main

import "testing"

func TestParseExposeEndpoint(t *testing.T) {
	tests := []struct {
		name        string
		input       string
		allowEmpty  bool
		wantInst    string
		wantPort    uint16
		wantPortSet bool
		wantErr     bool
	}{
		{name: "instance and port", input: "vm1:8080", wantInst: "vm1", wantPort: 8080, wantPortSet: true},
		{name: "instance only", input: "vm1", wantInst: "vm1"},
		{name: "host port only", input: ":9090", allowEmpty: true, wantInst: "", wantPort: 9090, wantPortSet: true},
		{name: "empty instance rejected", input: ":9090", wantErr: true},
		{name: "zero port rejected", input: "vm1:0", wantErr: true},
		{name: "invalid port rejected", input: "vm1:abc", wantErr: true},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			got, err := parseExposeEndpoint(tc.input, tc.allowEmpty)
			if tc.wantErr {
				if err == nil {
					t.Fatalf("expected error, got %+v", got)
				}
				return
			}
			if err != nil {
				t.Fatalf("unexpected error: %v", err)
			}
			if got.Instance != tc.wantInst || got.Port != tc.wantPort || got.PortSet != tc.wantPortSet {
				t.Fatalf("got %+v, want instance=%q port=%d portSet=%v", got, tc.wantInst, tc.wantPort, tc.wantPortSet)
			}
		})
	}
}

func TestResolveExposePorts(t *testing.T) {
	tests := []struct {
		name         string
		src          exposeEndpoint
		dst          exposeEndpoint
		wantSrcPort  uint16
		wantDstPort  uint16
		wantHostMode bool
		wantErr      bool
	}{
		{
			name:         "host defaults same port",
			src:          exposeEndpoint{Instance: "vm1", Port: 8080, PortSet: true},
			wantSrcPort:  8080,
			wantDstPort:  8080,
			wantHostMode: true,
		},
		{
			name:         "host explicit port",
			src:          exposeEndpoint{Instance: "vm1", Port: 8080, PortSet: true},
			dst:          exposeEndpoint{Port: 9090, PortSet: true},
			wantSrcPort:  8080,
			wantDstPort:  9090,
			wantHostMode: true,
		},
		{
			name:         "source inherits host port",
			src:          exposeEndpoint{Instance: "vm1"},
			dst:          exposeEndpoint{Port: 9090, PortSet: true},
			wantSrcPort:  9090,
			wantDstPort:  9090,
			wantHostMode: true,
		},
		{
			name:         "vm destination defaults to source port",
			src:          exposeEndpoint{Instance: "vm1", Port: 8080, PortSet: true},
			dst:          exposeEndpoint{Instance: "vm2"},
			wantSrcPort:  8080,
			wantDstPort:  8080,
			wantHostMode: false,
		},
		{
			name:         "source inherits vm destination port",
			src:          exposeEndpoint{Instance: "vm1"},
			dst:          exposeEndpoint{Instance: "vm2", Port: 9090, PortSet: true},
			wantSrcPort:  9090,
			wantDstPort:  9090,
			wantHostMode: false,
		},
		{
			name:    "missing all ports errors",
			src:     exposeEndpoint{Instance: "vm1"},
			dst:     exposeEndpoint{Instance: "vm2"},
			wantErr: true,
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			gotSrc, gotDst, gotHostMode, err := resolveExposePorts(tc.src, tc.dst)
			if tc.wantErr {
				if err == nil {
					t.Fatalf("expected error, got src=%d dst=%d hostMode=%v", gotSrc, gotDst, gotHostMode)
				}
				return
			}
			if err != nil {
				t.Fatalf("unexpected error: %v", err)
			}
			if gotSrc != tc.wantSrcPort || gotDst != tc.wantDstPort || gotHostMode != tc.wantHostMode {
				t.Fatalf("got src=%d dst=%d hostMode=%v, want src=%d dst=%d hostMode=%v", gotSrc, gotDst, gotHostMode, tc.wantSrcPort, tc.wantDstPort, tc.wantHostMode)
			}
		})
	}
}
