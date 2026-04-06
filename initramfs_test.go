package lnx

import (
	"bytes"
	"reflect"
	"testing"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestWriteCpioEntry_RegularFile(t *testing.T) {
	var buf bytes.Buffer
	data := []byte("hello world")
	writeCpioEntry(&buf, "init", data, 0100755)

	result := buf.Bytes()
	// cpio newc magic
	assert.Equal(t, "070701", string(result[:6]))
	// File size should be encoded at offset 54 (8 hex chars)
	assert.Equal(t, "0000000B", string(result[54:62]))
	// Name size should be 5 ("init" + null)
	assert.Equal(t, "00000005", string(result[94:102]))
}

func TestWriteCpioEntry_Trailer(t *testing.T) {
	var buf bytes.Buffer
	writeCpioEntry(&buf, "TRAILER!!!", nil, 0)

	result := buf.Bytes()
	assert.Equal(t, "070701", string(result[:6]))
	// File size should be 0
	assert.Equal(t, "00000000", string(result[54:62]))
}

func TestWriteInitramfs_RequiresInitBinary(t *testing.T) {
	original := InitBinary
	defer func() { InitBinary = original }()

	InitBinary = nil
	_, err := writeInitramfs(t.TempDir())
	require.Error(t, err)
	assert.Contains(t, err.Error(), "InitBinary not set")
}

func TestWriteInitramfs_ProducesValidCpio(t *testing.T) {
	original := InitBinary
	defer func() { InitBinary = original }()

	InitBinary = []byte("#!/bin/sh\necho hi\n")

	dir := t.TempDir()
	path, err := writeInitramfs(dir)
	require.NoError(t, err)
	assert.FileExists(t, path)
}

func TestConfig_Defaults(t *testing.T) {
	cfg := &Config{}
	assert.Equal(t, uint(2), cfg.cpus())
	// Default is 50% of host memory; just check it's reasonable (>= 1 GiB).
	assert.GreaterOrEqual(t, cfg.memoryBytes(), uint64(1<<30))
}

func TestConfig_CustomValues(t *testing.T) {
	cfg := &Config{CPUs: 4, MemoryBytes: 2 << 30}
	assert.Equal(t, uint(4), cfg.cpus())
	assert.Equal(t, uint64(2<<30), cfg.memoryBytes())
}

func TestConfig_AllFieldsCopied(t *testing.T) {
	// Verify that every Config field is represented in this test.
	// When a new field is added to Config, this test will fail
	// until the field is added here — catching ephemeral copy bugs.
	typ := reflect.TypeOf(Config{})
	fieldCount := typ.NumField()

	cfg := &Config{
		KernelPath:    "/kernel",
		RootfsPath:    "/rootfs",
		InitramfsPath: "/initrd",
		CPUs:          4,
		MemoryBytes:   8 << 30,
		CWD:           "/work",
		Env:           []string{"A=B"},
		Checkpoint:    true,
		CheckpointDir: "/cp",
		Shares:        []string{"/extra"},
		Hostname:      "test.lnx",
		SSHAgent:      true,
		GUI:           true,
		InitialHoldID: "hold-1",
		Ephemeral:     true,
		SocketDir:     "/sock",
	}

	// Count fields set above — must match struct field count.
	// If this fails, a new field was added to Config but not to this test.
	assert.Equal(t, fieldCount, 16, "Config has %d fields but test only covers 16 — update this test and the ephemeral copy in vm.go", fieldCount)

	// Verify all fields are non-zero (catches typos in field names above).
	val := reflect.ValueOf(*cfg)
	for i := 0; i < val.NumField(); i++ {
		f := val.Field(i)
		assert.False(t, f.IsZero(), "Config.%s is zero — add it to this test", typ.Field(i).Name)
	}
}
