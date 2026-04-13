// Package pack handles reading and extracting packed binary payloads.
// It provides zstd compression/decompression and SHA256 verification
// for kernel and rootfs blobs embedded in lnx binaries.
package pack

import (
	"crypto/sha256"
	"debug/macho"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"

	"github.com/klauspost/compress/zstd"
)

// Config is the configuration and blob metadata baked into a packed binary.
//
// When embedded in a Mach-O section, the layout is:
//
//	[u64 data_size]                 (0 if not packed)
//	[zstd-compressed kernel]        (KernelCompSize bytes)
//	[zstd-compressed rootfs]        (RootfsCompSize bytes)
//	[JSON Config]
//	[u64 json_len]
//	[zero padding to 16KB alignment]
type Config struct {
	Instance       string   `json:"instance"`
	Args           []string `json:"args"`
	KernelCompSize int64    `json:"kernel_comp_size"`
	RootfsCompSize int64    `json:"rootfs_comp_size"`
	KernelSHA256   string   `json:"kernel_sha256"`
	RootfsSHA256   string   `json:"rootfs_sha256"`

	// DataFileOffset is the file offset where kernel data starts.
	// Set at read time, not serialized.
	DataFileOffset int64 `json:"-"`
}

// ReadConfig reads the pack config from the current executable's
// Mach-O section (segName, sectName).
func ReadConfig(segName, sectName string) (*Config, error) {
	self, err := os.Executable()
	if err != nil {
		return nil, err
	}
	return ReadConfigFrom(self, sectName)
}

// ReadConfigFrom reads the pack config from the named Mach-O section.
func ReadConfigFrom(path, sectName string) (*Config, error) {
	f, err := macho.Open(path)
	if err != nil {
		return nil, fmt.Errorf("open macho: %w", err)
	}
	defer f.Close()

	sect := f.Section(sectName)
	if sect == nil {
		return nil, fmt.Errorf("no %s section", sectName)
	}

	var hdr [8]byte
	if _, err := sect.ReadAt(hdr[:], 0); err != nil {
		return nil, fmt.Errorf("read section header: %w", err)
	}
	dataSize := binary.LittleEndian.Uint64(hdr[:])
	if dataSize == 0 {
		return nil, fmt.Errorf("not a packed binary")
	}

	var jsonLenBuf [8]byte
	jsonLenOff := int64(8 + dataSize - 8)
	if _, err := sect.ReadAt(jsonLenBuf[:], jsonLenOff); err != nil {
		return nil, fmt.Errorf("read json_len: %w", err)
	}
	jsonLen := binary.LittleEndian.Uint64(jsonLenBuf[:])

	jsonOff := jsonLenOff - int64(jsonLen)
	jsonBytes := make([]byte, jsonLen)
	if _, err := sect.ReadAt(jsonBytes, jsonOff); err != nil {
		return nil, fmt.Errorf("read pack config: %w", err)
	}

	var cfg Config
	if err := json.Unmarshal(jsonBytes, &cfg); err != nil {
		return nil, fmt.Errorf("parse pack config: %w", err)
	}
	cfg.DataFileOffset = int64(sect.Offset) + 8
	return &cfg, nil
}

// EnsureFiles extracts the embedded kernel and rootfs to cacheDir,
// returning their paths. Skips extraction if already cached.
func EnsureFiles(cfg *Config, cacheDir string) (kernelPath, rootfsPath string, err error) {
	cacheID := cfg.KernelSHA256[:16] + "-" + cfg.RootfsSHA256[:16]
	dir := filepath.Join(cacheDir, cacheID)
	kernelPath = filepath.Join(dir, "vmlinuz")
	rootfsPath = filepath.Join(dir, "rootfs.ext4")

	kOK := FileExistsWithHash(kernelPath, cfg.KernelSHA256)
	rOK := FileExistsWithHash(rootfsPath, cfg.RootfsSHA256)
	if kOK && rOK {
		return kernelPath, rootfsPath, nil
	}

	if err := os.MkdirAll(dir, 0755); err != nil {
		return "", "", fmt.Errorf("create cache dir: %w", err)
	}

	self, err := os.Executable()
	if err != nil {
		return "", "", fmt.Errorf("find executable: %w", err)
	}

	f, err := os.Open(self)
	if err != nil {
		return "", "", fmt.Errorf("open self: %w", err)
	}
	defer f.Close()

	kernelStart := cfg.DataFileOffset
	rootfsStart := kernelStart + cfg.KernelCompSize

	if !kOK {
		fmt.Fprintln(os.Stderr, "extracting kernel...")
		if err := ExtractZstdBlob(f, kernelStart, cfg.KernelCompSize, kernelPath, cfg.KernelSHA256); err != nil {
			return "", "", fmt.Errorf("extract kernel: %w", err)
		}
	}

	if !rOK {
		fmt.Fprintln(os.Stderr, "extracting rootfs...")
		if err := ExtractZstdBlob(f, rootfsStart, cfg.RootfsCompSize, rootfsPath, cfg.RootfsSHA256); err != nil {
			return "", "", fmt.Errorf("extract rootfs: %w", err)
		}
	}

	return kernelPath, rootfsPath, nil
}

// ExtractZstdBlob decompresses a zstd blob from the given file section to dest,
// verifying the sha256 of the decompressed data.
func ExtractZstdBlob(f *os.File, offset, compSize int64, dest, wantSHA256 string) error {
	r := io.NewSectionReader(f, offset, compSize)

	dec, err := zstd.NewReader(r)
	if err != nil {
		return fmt.Errorf("zstd reader: %w", err)
	}
	defer dec.Close()

	tmp := dest + ".tmp"
	out, err := os.Create(tmp)
	if err != nil {
		return err
	}

	h := sha256.New()
	if _, err := io.Copy(io.MultiWriter(out, h), dec); err != nil {
		out.Close()
		os.Remove(tmp)
		return fmt.Errorf("decompress: %w", err)
	}
	if err := out.Close(); err != nil {
		os.Remove(tmp)
		return err
	}

	got := hex.EncodeToString(h.Sum(nil))
	if got != wantSHA256 {
		os.Remove(tmp)
		return fmt.Errorf("sha256 mismatch: got %s, want %s", got, wantSHA256)
	}

	return os.Rename(tmp, dest)
}

// FileExistsWithHash returns true if path exists and its sha256 matches want.
func FileExistsWithHash(path, wantHex string) bool {
	f, err := os.Open(path)
	if err != nil {
		return false
	}
	defer f.Close()
	h := sha256.New()
	if _, err := io.Copy(h, f); err != nil {
		return false
	}
	return hex.EncodeToString(h.Sum(nil)) == wantHex
}

// CompressZstdFile compresses src to dst using zstd, returning the
// sha256 of the uncompressed data and the number of compressed bytes written.
func CompressZstdFile(src, dst string, progress io.Writer) (sha256hex string, compressedSize int64, err error) {
	in, err := os.Open(src)
	if err != nil {
		return "", 0, err
	}
	defer in.Close()

	tmp := dst + ".tmp"
	out, err := os.Create(tmp)
	if err != nil {
		return "", 0, err
	}

	enc, err := zstd.NewWriter(out, zstd.WithEncoderLevel(zstd.SpeedBestCompression))
	if err != nil {
		out.Close()
		os.Remove(tmp)
		return "", 0, err
	}

	h := sha256.New()
	var r io.Reader = in
	if progress != nil {
		r = io.TeeReader(r, progress)
	}
	if _, err := io.Copy(io.MultiWriter(enc, h), r); err != nil {
		enc.Close()
		out.Close()
		os.Remove(tmp)
		return "", 0, fmt.Errorf("compress: %w", err)
	}
	if err := enc.Close(); err != nil {
		out.Close()
		os.Remove(tmp)
		return "", 0, err
	}
	pos, err := out.Seek(0, io.SeekCurrent)
	if err != nil {
		out.Close()
		os.Remove(tmp)
		return "", 0, err
	}
	if err := out.Close(); err != nil {
		os.Remove(tmp)
		return "", 0, err
	}
	if err := os.Rename(tmp, dst); err != nil {
		os.Remove(tmp)
		return "", 0, err
	}
	return hex.EncodeToString(h.Sum(nil)), pos, nil
}

// BuildBlob assembles the packed data blob from compressed files and config.
// Layout: [kernel.zst bytes][rootfs.zst bytes][JSON config][u64 json_len]
func BuildBlob(kernelComp, rootfsComp string, cfg *Config) ([]byte, error) {
	kernelData, err := os.ReadFile(kernelComp)
	if err != nil {
		return nil, fmt.Errorf("read kernel: %w", err)
	}
	rootfsData, err := os.ReadFile(rootfsComp)
	if err != nil {
		return nil, fmt.Errorf("read rootfs: %w", err)
	}
	jsonBytes, err := json.Marshal(cfg)
	if err != nil {
		return nil, err
	}

	jsonLen := make([]byte, 8)
	binary.LittleEndian.PutUint64(jsonLen, uint64(len(jsonBytes)))

	blob := make([]byte, 0, len(kernelData)+len(rootfsData)+len(jsonBytes)+8)
	blob = append(blob, kernelData...)
	blob = append(blob, rootfsData...)
	blob = append(blob, jsonBytes...)
	blob = append(blob, jsonLen...)
	return blob, nil
}
