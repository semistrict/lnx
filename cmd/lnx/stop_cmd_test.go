package main

import (
	"bytes"
	"testing"
)

func TestWriteTerminalStatusLine(t *testing.T) {
	var buf bytes.Buffer
	writeTerminalStatusLine(&buf, "VM stopping. Press k to kill.", true)
	if got, want := buf.String(), "VM stopping. Press k to kill.\r\n"; got != want {
		t.Fatalf("raw line = %q, want %q", got, want)
	}

	buf.Reset()
	writeTerminalStatusLine(&buf, "VM stopping.", false)
	if got, want := buf.String(), "VM stopping.\n"; got != want {
		t.Fatalf("normal line = %q, want %q", got, want)
	}
}

func TestShouldForceKillInput(t *testing.T) {
	for _, input := range []string{"k\n", "K\n", "  k  \n"} {
		if !shouldForceKillInput(input) {
			t.Fatalf("shouldForceKillInput(%q) = false, want true", input)
		}
	}
	for _, input := range []string{"", "\n", "x\n", "kk\n"} {
		if shouldForceKillInput(input) {
			t.Fatalf("shouldForceKillInput(%q) = true, want false", input)
		}
	}
}
