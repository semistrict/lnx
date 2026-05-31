package main

import (
	"encoding/binary"
	"encoding/json"
	"os"
	"testing"

	"github.com/semistrict/lnx/internal/pack"
)

// buildTestMacho creates a minimal Mach-O 64-bit binary with a __LNX,__lnxpack
// section suitable for testing machoInjectSection.
func buildTestMacho(t *testing.T) []byte {
	t.Helper()

	const pageSize = 16384
	// Layout:
	//   [mach_header_64]           0..32
	//   [LC_SEGMENT_64 __TEXT]     32..104   (covers header + load cmds)
	//   [LC_SEGMENT_64 __LNX]     104..256  (72 + 80 = 152, but cmdsize=152)
	//   [LC_SEGMENT_64 __LINKEDIT] 256..328
	//   [LC_SYMTAB]                328..352
	//   [LC_CODE_SIGNATURE]        352..368
	//   ... padding to pageSize ...
	//   [__TEXT data]              0..pageSize  (the header IS __TEXT)
	//   [__LNX,__lnxpack data]    pageSize..2*pageSize
	//   [__LINKEDIT data]         2*pageSize..3*pageSize
	//
	// Total: 3 pages = 3*16384 = 49152

	ncmds := uint32(5)
	sizeOfCmds := uint32(72 + (72 + 80) + 72 + 24 + 16) // TEXT + LNX(seg+sect) + LINKEDIT + SYMTAB + CODESIG
	totalSize := 3 * pageSize

	bin := make([]byte, totalSize)

	// mach_header_64
	binary.LittleEndian.PutUint32(bin[0:], 0xFEEDFACF)  // magic
	binary.LittleEndian.PutUint32(bin[4:], 0x0100000C)   // CPU_TYPE_ARM64
	binary.LittleEndian.PutUint32(bin[8:], 0x00000000)   // cpusubtype
	binary.LittleEndian.PutUint32(bin[12:], 2)            // MH_EXECUTE
	binary.LittleEndian.PutUint32(bin[16:], ncmds)        // ncmds
	binary.LittleEndian.PutUint32(bin[20:], sizeOfCmds)   // sizeofcmds
	binary.LittleEndian.PutUint32(bin[24:], 0)            // flags
	binary.LittleEndian.PutUint32(bin[28:], 0)            // reserved

	off := 32

	// LC_SEGMENT_64: __TEXT (covers the entire first page including header)
	binary.LittleEndian.PutUint32(bin[off+0:], 0x19)   // cmd = LC_SEGMENT_64
	binary.LittleEndian.PutUint32(bin[off+4:], 72)     // cmdsize
	copy(bin[off+8:], "__TEXT\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00")
	binary.LittleEndian.PutUint64(bin[off+24:], 0x100000000)     // vmaddr
	binary.LittleEndian.PutUint64(bin[off+32:], uint64(pageSize)) // vmsize
	binary.LittleEndian.PutUint64(bin[off+40:], 0)                // fileoff
	binary.LittleEndian.PutUint64(bin[off+48:], uint64(pageSize)) // filesize
	binary.LittleEndian.PutUint32(bin[off+56:], 5)                // maxprot (r+x)
	binary.LittleEndian.PutUint32(bin[off+60:], 5)                // initprot
	binary.LittleEndian.PutUint32(bin[off+64:], 0)                // nsects
	binary.LittleEndian.PutUint32(bin[off+68:], 0)                // flags
	off += 72

	// LC_SEGMENT_64: __LNX with 1 section __lnxpack
	lnxSegOff := off
	binary.LittleEndian.PutUint32(bin[off+0:], 0x19)     // cmd
	binary.LittleEndian.PutUint32(bin[off+4:], 72+80)    // cmdsize (seg + 1 section)
	copy(bin[off+8:], "__LNX\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00")
	binary.LittleEndian.PutUint64(bin[off+24:], 0x100000000+uint64(pageSize)) // vmaddr
	binary.LittleEndian.PutUint64(bin[off+32:], uint64(pageSize))             // vmsize
	binary.LittleEndian.PutUint64(bin[off+40:], uint64(pageSize))             // fileoff
	binary.LittleEndian.PutUint64(bin[off+48:], uint64(pageSize))             // filesize
	binary.LittleEndian.PutUint32(bin[off+56:], 3)                            // maxprot (r+w)
	binary.LittleEndian.PutUint32(bin[off+60:], 3)                            // initprot
	binary.LittleEndian.PutUint32(bin[off+64:], 1)                            // nsects
	binary.LittleEndian.PutUint32(bin[off+68:], 0)                            // flags
	off += 72
	_ = lnxSegOff

	// section_64: __lnxpack
	copy(bin[off+0:], "__lnxpack\x00\x00\x00\x00\x00\x00\x00")
	copy(bin[off+16:], "__LNX\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00")
	binary.LittleEndian.PutUint64(bin[off+32:], 0x100000000+uint64(pageSize)) // addr
	binary.LittleEndian.PutUint64(bin[off+40:], uint64(pageSize))             // size
	binary.LittleEndian.PutUint32(bin[off+48:], uint32(pageSize))             // offset
	binary.LittleEndian.PutUint32(bin[off+52:], 14)                           // align = 2^14 = 16384
	off += 80

	// LC_SEGMENT_64: __LINKEDIT
	binary.LittleEndian.PutUint32(bin[off+0:], 0x19)     // cmd
	binary.LittleEndian.PutUint32(bin[off+4:], 72)       // cmdsize
	copy(bin[off+8:], "__LINKEDIT\x00\x00\x00\x00\x00\x00")
	binary.LittleEndian.PutUint64(bin[off+24:], 0x100000000+2*uint64(pageSize)) // vmaddr
	binary.LittleEndian.PutUint64(bin[off+32:], uint64(pageSize))               // vmsize
	binary.LittleEndian.PutUint64(bin[off+40:], 2*uint64(pageSize))             // fileoff
	binary.LittleEndian.PutUint64(bin[off+48:], uint64(pageSize))               // filesize
	binary.LittleEndian.PutUint32(bin[off+56:], 1)                              // maxprot (r)
	binary.LittleEndian.PutUint32(bin[off+60:], 1)                              // initprot
	off += 72

	// LC_SYMTAB
	binary.LittleEndian.PutUint32(bin[off+0:], 0x2) // cmd = LC_SYMTAB
	binary.LittleEndian.PutUint32(bin[off+4:], 24)  // cmdsize
	binary.LittleEndian.PutUint32(bin[off+8:], uint32(2*pageSize+100))  // symoff (in LINKEDIT)
	binary.LittleEndian.PutUint32(bin[off+12:], 0)  // nsyms
	binary.LittleEndian.PutUint32(bin[off+16:], uint32(2*pageSize+200)) // stroff (in LINKEDIT)
	binary.LittleEndian.PutUint32(bin[off+20:], 0)  // strsize
	off += 24

	// LC_CODE_SIGNATURE
	binary.LittleEndian.PutUint32(bin[off+0:], 0x1D) // cmd = LC_CODE_SIGNATURE
	binary.LittleEndian.PutUint32(bin[off+4:], 16)   // cmdsize
	binary.LittleEndian.PutUint32(bin[off+8:], uint32(2*pageSize+8000)) // dataoff
	binary.LittleEndian.PutUint32(bin[off+12:], 1000) // datasize

	return bin
}

func TestMachoInjectSection(t *testing.T) {
	src := buildTestMacho(t)
	const pageSize = 16384

	blob := []byte("test-kernel-datarootfs-data")
	cfg := &pack.Config{
		Instance:       "test",
		Args:           []string{"echo", "hello"},
		KernelCompSize: 16,
		RootfsCompSize: 10,
		KernelSHA256:   "aaaa",
		RootfsSHA256:   "bbbb",
	}
	jsonBytes, _ := json.Marshal(cfg)
	jsonLen := make([]byte, 8)
	binary.LittleEndian.PutUint64(jsonLen, uint64(len(jsonBytes)))

	fullBlob := make([]byte, 0)
	fullBlob = append(fullBlob, blob...)
	fullBlob = append(fullBlob, jsonBytes...)
	fullBlob = append(fullBlob, jsonLen...)

	out, err := machoInjectSection(src, fullBlob)
	if err != nil {
		t.Fatalf("machoInjectSection: %v", err)
	}

	// The output should be larger than input (blob > 16KB placeholder).
	alignedBlobSize := machoAlignUp(8+uint64(len(fullBlob)), 16384)
	expectedSize := int(pageSize) + int(alignedBlobSize) + int(pageSize)
	if len(out) != expectedSize {
		t.Errorf("output size: got %d, want %d", len(out), expectedSize)
	}

	// Verify __LNX segment was resized.
	lnxFilesize := binary.LittleEndian.Uint64(out[104+48:]) // segment filesize at known offset
	if lnxFilesize != alignedBlobSize {
		t.Errorf("__LNX filesize: got %d, want %d", lnxFilesize, alignedBlobSize)
	}

	// Verify __LINKEDIT was shifted.
	linkeditOff := 104 + 72 + 80 // after __TEXT(72) + __LNX(72+80) segments
	linkeditFileoff := binary.LittleEndian.Uint64(out[linkeditOff+40:])
	expectedLinkeditFileoff := uint64(pageSize) + alignedBlobSize
	if linkeditFileoff != expectedLinkeditFileoff {
		t.Errorf("__LINKEDIT fileoff: got %d, want %d", linkeditFileoff, expectedLinkeditFileoff)
	}

	// Verify data_size header was written.
	dataSizeOff := pageSize // __LNX section starts at page 1
	dataSize := binary.LittleEndian.Uint64(out[dataSizeOff:])
	if dataSize != uint64(len(fullBlob)) {
		t.Errorf("data_size: got %d, want %d", dataSize, len(fullBlob))
	}

	// Verify blob data was written.
	for i, b := range fullBlob {
		if out[dataSizeOff+8+i] != b {
			t.Errorf("blob[%d]: got 0x%x, want 0x%x", i, out[dataSizeOff+8+i], b)
			break
		}
	}

	// Verify SYMTAB offsets were shifted.
	symtabOff := linkeditOff + 72
	symoff := binary.LittleEndian.Uint32(out[symtabOff+8:])
	expectedSymoff := uint32(int64(2*pageSize+100) + int64(alignedBlobSize) - int64(pageSize))
	if symoff != expectedSymoff {
		t.Errorf("symoff: got %d, want %d", symoff, expectedSymoff)
	}
}

func TestMachoInjectAndReadConfig(t *testing.T) {
	src := buildTestMacho(t)

	cfg := &pack.Config{
		Instance:       "myinstance",
		Args:           []string{"bash", "-c", "echo hello"},
		KernelCompSize: 17,
		RootfsCompSize: 17,
		KernelSHA256:   "aaaa",
		RootfsSHA256:   "bbbb",
	}

	// Build a blob with fake kernel/rootfs data.
	kernel := []byte("compressed-kernel")
	rootfs := []byte("compressed-rootfs")
	jsonBytes, _ := json.Marshal(cfg)
	jsonLen := make([]byte, 8)
	binary.LittleEndian.PutUint64(jsonLen, uint64(len(jsonBytes)))

	blob := make([]byte, 0, len(kernel)+len(rootfs)+len(jsonBytes)+8)
	blob = append(blob, kernel...)
	blob = append(blob, rootfs...)
	blob = append(blob, jsonBytes...)
	blob = append(blob, jsonLen...)

	out, err := machoInjectSection(src, blob)
	if err != nil {
		t.Fatalf("machoInjectSection: %v", err)
	}

	// Write to a temp file and read config back.
	tmp := t.TempDir()
	path := tmp + "/packed"
	if err := os.WriteFile(path, out, 0755); err != nil {
		t.Fatal(err)
	}

	got, err := readPackedConfigFrom(path)
	if err != nil {
		t.Fatalf("readPackedConfigFrom: %v", err)
	}
	if got.Instance != cfg.Instance {
		t.Errorf("instance: got %q, want %q", got.Instance, cfg.Instance)
	}
	if len(got.Args) != len(cfg.Args) {
		t.Fatalf("args len: got %d, want %d", len(got.Args), len(cfg.Args))
	}
	for i, a := range cfg.Args {
		if got.Args[i] != a {
			t.Errorf("args[%d]: got %q, want %q", i, got.Args[i], a)
		}
	}
	if got.KernelCompSize != cfg.KernelCompSize {
		t.Errorf("kernel_comp_size: got %d, want %d", got.KernelCompSize, cfg.KernelCompSize)
	}
	if got.RootfsCompSize != cfg.RootfsCompSize {
		t.Errorf("rootfs_comp_size: got %d, want %d", got.RootfsCompSize, cfg.RootfsCompSize)
	}
	if got.DataFileOffset == 0 {
		t.Error("dataFileOffset should be non-zero")
	}
}

func TestReadPackedConfigNoSection(t *testing.T) {
	// A file without a Mach-O header should return an error.
	tmp := t.TempDir()
	p := tmp + "/plain"
	if err := os.WriteFile(p, []byte("just a regular binary"), 0755); err != nil {
		t.Fatal(err)
	}
	if _, err := readPackedConfigFrom(p); err == nil {
		t.Error("expected error for non-Mach-O file, got nil")
	}
}

func TestReadPackedConfigUnpacked(t *testing.T) {
	// A Mach-O with the section but data_size=0 should return an error.
	src := buildTestMacho(t)
	tmp := t.TempDir()
	p := tmp + "/unpacked"
	if err := os.WriteFile(p, src, 0755); err != nil {
		t.Fatal(err)
	}
	_, err := readPackedConfigFrom(p)
	if err == nil {
		t.Error("expected error for unpacked Mach-O, got nil")
	}
}
