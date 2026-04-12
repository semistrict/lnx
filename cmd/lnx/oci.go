package main

import (
	"archive/tar"
	"bufio"
	"compress/gzip"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"log/slog"
	"os"
	"path"
	"path/filepath"
	"strings"

	"github.com/google/go-containerregistry/pkg/name"
	v1 "github.com/google/go-containerregistry/pkg/v1"
	"github.com/google/go-containerregistry/pkg/v1/remote"
	"github.com/semistrict/go2fs"
	"golang.org/x/sys/unix"
)

// ociImage wraps a pulled OCI image with its layer info.
type ociImage struct {
	image  v1.Image
	layers []v1.Layer
	config *v1.ConfigFile
}

// defaultCmd returns the command to run from the image config (Entrypoint + Cmd).
func (img *ociImage) defaultCmd() []string {
	cfg := img.config.Config
	var args []string
	args = append(args, cfg.Entrypoint...)
	args = append(args, cfg.Cmd...)
	return args
}

// pullImage fetches an OCI image from a registry and caches its layer blobs.
func pullImage(ref string) (*ociImage, error) {
	parsed, err := name.ParseReference(ref)
	if err != nil {
		return nil, fmt.Errorf("parse reference %q: %w", ref, err)
	}

	platform := v1.Platform{
		Architecture: "arm64",
		OS:           "linux",
	}

	desc, err := remote.Get(parsed, remote.WithPlatform(platform))
	if err != nil {
		return nil, fmt.Errorf("fetch descriptor: %w", err)
	}

	img, err := desc.Image()
	if err != nil {
		return nil, fmt.Errorf("resolve image: %w", err)
	}

	layers, err := img.Layers()
	if err != nil {
		return nil, fmt.Errorf("get layers: %w", err)
	}

	config, err := img.ConfigFile()
	if err != nil {
		return nil, fmt.Errorf("get config: %w", err)
	}

	// Download layer blobs (compressed).
	for i, layer := range layers {
		digest, err := layer.Digest()
		if err != nil {
			return nil, fmt.Errorf("layer %d digest: %w", i, err)
		}

		blobPath := filepath.Join(ociBlobDir(), digest.Hex)
		if _, err := os.Stat(blobPath); err == nil {
			fmt.Fprintf(os.Stderr, "  layer %d: %s (cached)\n", i, digest.Hex[:12])
			continue
		}

		size, _ := layer.Size()
		fmt.Fprintf(os.Stderr, "  layer %d: %s (%.1f MB)\n", i, digest.Hex[:12], float64(size)/(1024*1024))

		rc, err := layer.Compressed()
		if err != nil {
			return nil, fmt.Errorf("layer %d read: %w", i, err)
		}

		tmp := blobPath + ".tmp"
		f, err := os.Create(tmp)
		if err != nil {
			rc.Close()
			return nil, fmt.Errorf("create blob: %w", err)
		}
		if _, err := io.Copy(f, rc); err != nil {
			f.Close()
			rc.Close()
			os.Remove(tmp)
			return nil, fmt.Errorf("download layer %d: %w", i, err)
		}
		f.Close()
		rc.Close()

		if err := os.Rename(tmp, blobPath); err != nil {
			os.Remove(tmp)
			return nil, err
		}
	}

	return &ociImage{image: img, layers: layers, config: config}, nil
}

// chainID computes the chain ID for a sequence of layer diff IDs.
// chainID(L0) = diffID(L0)
// chainID(L0, L1) = sha256(chainID(L0) + " " + diffID(L1))
func chainID(diffIDs []v1.Hash) string {
	if len(diffIDs) == 0 {
		return ""
	}
	chain := diffIDs[0].Hex
	for i := 1; i < len(diffIDs); i++ {
		h := sha256.Sum256([]byte("sha256:" + chain + " " + diffIDs[i].String()))
		chain = hex.EncodeToString(h[:])
	}
	return chain
}

const defaultImageSize = 4 * 1024 * 1024 * 1024 // 4 GB sparse

// buildLayers creates cumulative ext4 snapshots for each layer using APFS
// clonefile between layers and go2fs for direct ext4 writes (no VM needed).
// Returns the path to the final layer's ext4 file.
func buildLayers(img *ociImage) (string, error) {
	var diffIDs []v1.Hash
	for i, layer := range img.layers {
		diffID, err := layer.DiffID()
		if err != nil {
			return "", fmt.Errorf("layer %d diff ID: %w", i, err)
		}
		diffIDs = append(diffIDs, diffID)
	}

	var prevPath string
	var finalPath string

	for i, layer := range img.layers {
		cid := chainID(diffIDs[:i+1])
		layerPath := filepath.Join(ociLayerDir(), cid+".ext4")

		if _, err := os.Stat(layerPath); err == nil {
			fmt.Fprintf(os.Stderr, "  layer %d/%d: %s (cached)\n", i+1, len(img.layers), cid[:12])
			prevPath = layerPath
			finalPath = layerPath
			continue
		}

		fmt.Fprintf(os.Stderr, "  layer %d/%d: %s", i+1, len(img.layers), cid[:12])

		tmp := layerPath + ".tmp"

		if prevPath != "" {
			// Clone the previous layer's ext4 and apply the diff on top.
			if err := unix.Clonefile(prevPath, tmp, 0); err != nil {
				return "", fmt.Errorf("clonefile layer %d: %w", i, err)
			}
			fmt.Fprintf(os.Stderr, " (cloned)\n")
		} else {
			// First layer: create a fresh ext4 image.
			if err := createEmptyExt4(tmp, defaultImageSize); err != nil {
				return "", fmt.Errorf("create base ext4: %w", err)
			}
			fmt.Fprintf(os.Stderr, "\n")
		}

		// Open the ext4 for writing and apply the layer tar.
		digest, err := layer.Digest()
		if err != nil {
			os.Remove(tmp)
			return "", fmt.Errorf("layer %d digest: %w", i, err)
		}

		blobPath := filepath.Join(ociBlobDir(), digest.Hex)
		if err := applyLayerToExt4(tmp, blobPath); err != nil {
			os.Remove(tmp)
			return "", fmt.Errorf("apply layer %d: %w", i, err)
		}

		if err := os.Rename(tmp, layerPath); err != nil {
			os.Remove(tmp)
			return "", err
		}

		prevPath = layerPath
		finalPath = layerPath
	}

	return finalPath, nil
}

// createEmptyExt4 creates a sparse ext4 filesystem image.
func createEmptyExt4(path string, sizeBytes uint64) error {
	fs, err := go2fs.Create(path, sizeBytes)
	if err != nil {
		return err
	}
	return fs.Close()
}

// applyLayerToExt4 opens an existing ext4 image and applies an OCI layer
// tar (compressed) to it using go2fs — pure Go, no mount or VM required.
func applyLayerToExt4(ext4Path, blobPath string) error {
	f, err := os.Open(blobPath)
	if err != nil {
		return fmt.Errorf("open blob: %w", err)
	}
	defer f.Close()

	// Get the ext4 image size for re-opening.
	info, err := os.Stat(ext4Path)
	if err != nil {
		return err
	}

	fs, err := go2fs.Create(ext4Path, uint64(info.Size()))
	if err != nil {
		return fmt.Errorf("open ext4: %w", err)
	}
	defer fs.Close()

	// Auto-detect gzip.
	br := bufio.NewReader(f)
	var r io.Reader = br
	if peek, err := br.Peek(2); err == nil && peek[0] == 0x1f && peek[1] == 0x8b {
		gz, err := gzip.NewReader(br)
		if err != nil {
			return fmt.Errorf("gzip: %w", err)
		}
		defer gz.Close()
		r = gz
	}

	return applyTarToFS(fs, tar.NewReader(r))
}

// applyTarToFS writes tar entries to an ext4 filesystem via go2fs.
// Handles OCI whiteout files (.wh.*) by skipping them (deletion not
// yet supported by go2fs — correct for fresh builds where earlier
// layers don't conflict).
func applyTarToFS(fs *go2fs.FS, tr *tar.Reader) error {
	var dirs, files, symlinks, hardlinks, devs, skipped int

	for {
		hdr, err := tr.Next()
		if err == io.EOF {
			break
		}
		if err != nil {
			return fmt.Errorf("read tar header: %w", err)
		}

		name := path.Clean(hdr.Name)
		name = strings.TrimPrefix(name, "/")
		name = strings.TrimPrefix(name, "./")
		if name == "" || name == "." {
			continue
		}

		// Skip OCI whiteout markers (deletion not supported yet).
		base := path.Base(name)
		if strings.HasPrefix(base, ".wh.") {
			continue
		}

		uid := uint32(hdr.Uid)
		gid := uint32(hdr.Gid)
		mtime := hdr.ModTime.Unix()
		mode := uint32(hdr.Mode & 07777)

		switch hdr.Typeflag {
		case tar.TypeDir:
			if err := fs.Mkdir(name, mode, uid, gid, mtime); err != nil {
				// Directories may already exist from a previous layer.
				slog.Debug("mkdir (may exist)", "path", name, "error", err)
			}
			dirs++

		case tar.TypeReg:
			data, err := io.ReadAll(tr)
			if err != nil {
				return fmt.Errorf("read %q: %w", name, err)
			}
			if err := fs.WriteFile(name, mode, uid, gid, mtime, data); err != nil {
				return fmt.Errorf("write %q: %w", name, err)
			}
			files++

		case tar.TypeSymlink:
			if err := fs.Symlink(name, hdr.Linkname, uid, gid, mtime); err != nil {
				return fmt.Errorf("symlink %q -> %q: %w", name, hdr.Linkname, err)
			}
			symlinks++

		case tar.TypeLink:
			target := path.Clean(hdr.Linkname)
			target = strings.TrimPrefix(target, "/")
			target = strings.TrimPrefix(target, "./")
			if err := fs.Hardlink(name, target); err != nil {
				return fmt.Errorf("hardlink %q -> %q: %w", name, hdr.Linkname, err)
			}
			hardlinks++

		case tar.TypeChar:
			if err := fs.Mknod(name, 0020000|mode, uid, gid, mtime,
				uint32(hdr.Devmajor), uint32(hdr.Devminor)); err != nil {
				return fmt.Errorf("mknod char %q: %w", name, err)
			}
			devs++

		case tar.TypeBlock:
			if err := fs.Mknod(name, 0060000|mode, uid, gid, mtime,
				uint32(hdr.Devmajor), uint32(hdr.Devminor)); err != nil {
				return fmt.Errorf("mknod block %q: %w", name, err)
			}
			devs++

		case tar.TypeFifo:
			if err := fs.Mknod(name, 0010000|mode, uid, gid, mtime, 0, 0); err != nil {
				return fmt.Errorf("mknod fifo %q: %w", name, err)
			}
			devs++

		default:
			skipped++
		}
	}

	fmt.Fprintf(os.Stderr, "    %d dirs, %d files, %d symlinks, %d hardlinks\n",
		dirs, files, symlinks, hardlinks)
	return nil
}
