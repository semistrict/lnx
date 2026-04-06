package main

import (
	"bufio"
	"context"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/google/go-containerregistry/pkg/name"
	v1 "github.com/google/go-containerregistry/pkg/v1"
	"github.com/google/go-containerregistry/pkg/v1/remote"
	"github.com/semistrict/lnx"
)

var dockerPlatform = v1.Platform{
	OS:           "linux",
	Architecture: "arm64",
}

func ensureDockerImage(ref string) (*dockerImageMetadata, error) {
	if err := ensureDockerDirs(); err != nil {
		return nil, err
	}
	if meta, err := findLocalDockerImage(ref); err == nil {
		meta.LastUsedAt = time.Now()
		_ = saveDockerImage(meta)
		if _, err := os.Stat(dockerImageRootfsPath(meta.Digest)); err == nil {
			return meta, nil
		}
	}

	parsed, err := name.ParseReference(ref, name.WithDefaultRegistry(name.DefaultRegistry), name.WithDefaultTag("latest"))
	if err != nil {
		return nil, fmt.Errorf("parse image reference %q: %w", ref, err)
	}

	img, err := remote.Image(parsed, remote.WithPlatform(dockerPlatform))
	if err != nil {
		return nil, fmt.Errorf("pull image %s: %w", parsed.Name(), err)
	}

	digest, err := img.Digest()
	if err != nil {
		return nil, fmt.Errorf("image digest: %w", err)
	}
	digestStr := digest.String()

	if meta, err := loadDockerImage(digestStr); err == nil {
		meta.LastUsedAt = time.Now()
		_ = saveDockerImage(meta)
		if _, err := os.Stat(dockerImageRootfsPath(digestStr)); err == nil {
			return meta, nil
		}
	}

	cfg, err := img.ConfigFile()
	if err != nil {
		return nil, fmt.Errorf("image config: %w", err)
	}
	layers, err := img.Layers()
	if err != nil {
		return nil, fmt.Errorf("image layers: %w", err)
	}

	meta := &dockerImageMetadata{
		Reference:    parsed.Name(),
		CanonicalRef: parsed.Context().Digest(digestStr).Name(),
		Digest:       digestStr,
		Layers:       make([]string, 0, len(layers)),
		Config: dockerImageConfig{
			Env:        append([]string(nil), cfg.Config.Env...),
			Cmd:        append([]string(nil), cfg.Config.Cmd...),
			Entrypoint: append([]string(nil), cfg.Config.Entrypoint...),
			WorkingDir: cfg.Config.WorkingDir,
			Exposed:    exposedPorts(cfg.Config.ExposedPorts),
		},
		CreatedAt:  time.Now(),
		LastUsedAt: time.Now(),
	}

	imgDir := dockerImageDir(meta.Digest)
	if err := os.MkdirAll(imgDir, 0755); err != nil {
		return nil, err
	}

	layerListPath := dockerImageLayersPath(meta.Digest)
	layerListFile, err := os.Create(layerListPath)
	if err != nil {
		return nil, err
	}
	defer layerListFile.Close()
	layerList := bufio.NewWriter(layerListFile)

	for _, layer := range layers {
		layerDigest, err := layer.Digest()
		if err != nil {
			return nil, fmt.Errorf("layer digest: %w", err)
		}
		layerPath := filepath.Join(dockerBlobsDir(), strings.TrimPrefix(layerDigest.String(), "sha256:")+".tar.gz")
		if err := cacheLayerBlob(layerPath, layer); err != nil {
			return nil, err
		}
		meta.Layers = append(meta.Layers, layerDigest.String())
		if _, err := fmt.Fprintln(layerList, layerPath); err != nil {
			return nil, err
		}
	}
	if err := layerList.Flush(); err != nil {
		return nil, err
	}

	if err := saveDockerImage(meta); err != nil {
		return nil, err
	}
	if err := materializeDockerRootfs(meta); err != nil {
		return nil, err
	}
	return meta, nil
}

func findLocalDockerImage(ref string) (*dockerImageMetadata, error) {
	entries, err := os.ReadDir(dockerImagesDir())
	if err != nil {
		return nil, err
	}
	candidates := dockerReferenceCandidates(ref)
	for _, entry := range entries {
		if !entry.IsDir() {
			continue
		}
		meta, err := loadDockerImage(filepath.Base(entry.Name()))
		if err != nil {
			continue
		}
		existing := dockerReferenceCandidates(meta.Reference)
		existing = append(existing, meta.CanonicalRef)
		for _, candidate := range candidates {
			for _, have := range existing {
				if candidate != "" && candidate == have {
					return meta, nil
				}
			}
		}
	}
	return nil, fmt.Errorf("local image not found: %s", ref)
}

func dockerReferenceCandidates(ref string) []string {
	candidates := []string{ref}
	parsed, err := name.ParseReference(ref, name.WithDefaultRegistry(name.DefaultRegistry), name.WithDefaultTag("latest"))
	if err == nil {
		candidates = append(candidates, parsed.Name())
	}
	return candidates
}

func cacheLayerBlob(dst string, layer v1.Layer) error {
	if _, err := os.Stat(dst); err == nil {
		return nil
	}
	rc, err := layer.Compressed()
	if err != nil {
		return fmt.Errorf("open layer blob: %w", err)
	}
	defer rc.Close()

	if err := os.MkdirAll(filepath.Dir(dst), 0755); err != nil {
		return err
	}
	tmp := dst + ".tmp"
	f, err := os.Create(tmp)
	if err != nil {
		return err
	}
	if _, err := io.Copy(f, rc); err != nil {
		_ = f.Close()
		_ = os.Remove(tmp)
		return fmt.Errorf("cache layer blob: %w", err)
	}
	if err := f.Close(); err != nil {
		_ = os.Remove(tmp)
		return err
	}
	return os.Rename(tmp, dst)
}

func materializeDockerRootfs(meta *dockerImageMetadata) error {
	rootfsPath := dockerImageRootfsPath(meta.Digest)
	if _, err := os.Stat(rootfsPath); err == nil {
		return nil
	}

	defaultRootfs := filepath.Join(lnxBase(), "instances", "default", "rootfs.ext4")
	if _, err := os.Stat(defaultRootfs); err != nil {
		return fmt.Errorf("default instance rootfs is required to build image snapshots: %w", err)
	}
	kernelPath := filepath.Join(lnxBase(), "vmlinuz")
	if _, err := os.Stat(kernelPath); err != nil {
		return fmt.Errorf("kernel is required to build image snapshots: %w", err)
	}

	workdir, err := os.MkdirTemp("", "lnx-docker-build-*")
	if err != nil {
		return err
	}
	defer os.RemoveAll(workdir)

	layersSrc, err := os.ReadFile(dockerImageLayersPath(meta.Digest))
	if err != nil {
		return err
	}
	if err := os.WriteFile(filepath.Join(workdir, "layers.txt"), layersSrc, 0644); err != nil {
		return err
	}
	outputPath := filepath.Join(workdir, "rootfs.ext4")
	scriptPath := filepath.Join(workdir, "build-rootfs.sh")
	if err := os.WriteFile(scriptPath, []byte(dockerRootfsBuildScript(workdir, outputPath)), 0755); err != nil {
		return err
	}

	lnx.InitBinary = initBinary
	exitCode, err := lnx.Run(&lnx.Config{
		KernelPath: kernelPath,
		RootfsPath: defaultRootfs,
		CWD:        workdir,
		Ephemeral:  true,
		Hostname:   "docker-build.lnx",
	}, "sudo", "bash", scriptPath)
	if err != nil {
		return fmt.Errorf("build image rootfs: %w", err)
	}
	if exitCode != 0 {
		return fmt.Errorf("build image rootfs exited with status %d", exitCode)
	}
	return os.Rename(outputPath, rootfsPath)
}

func dockerRootfsBuildScript(workdir, rootfsPath string) string {
	return fmt.Sprintf(`#!/bin/bash
set -euo pipefail

workdir=%q
layers_file="$workdir/layers.txt"
out_image=%q
root_dir=$(mktemp -d /tmp/lnx-image-rootfs.XXXXXX)
tmp_image=$(mktemp /tmp/lnx-image-rootfs.ext4.XXXXXX)
cleanup() {
  rm -rf "$root_dir" "$tmp_image"
}
trap cleanup EXIT

while IFS= read -r layer; do
  [ -n "$layer" ] || continue
  while IFS= read -r entry; do
    [ -n "$entry" ] || continue
    base=$(basename "$entry")
    dir=$(dirname "$entry")
    case "$base" in
      .wh..wh..opq)
        target="$root_dir"
        if [ "$dir" != "." ]; then
          target="$root_dir/$dir"
        fi
        if [ -d "$target" ]; then
          find "$target" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
        fi
        ;;
      .wh.*)
        target="$root_dir"
        if [ "$dir" != "." ]; then
          target="$root_dir/$dir"
        fi
        rm -rf "$target/${base#.wh.}"
        ;;
    esac
  done < <(tar -tf "$layer")

  tar --exclude='.wh.*' --exclude='*/.wh.*' -xf "$layer" -C "$root_dir"
done < "$layers_file"

size_kb=$(du -sk "$root_dir" | awk '{print $1}')
img_kb=$((size_kb + size_kb / 5 + 262144))
truncate -s "${img_kb}K" "$tmp_image"
mke2fs -q -t ext4 -d "$root_dir" -L lnx-container "$tmp_image"
cp "$tmp_image" "$out_image"
`, workdir, rootfsPath)
}

func exposedPorts(in map[string]struct{}) map[string]uint16 {
	if len(in) == 0 {
		return nil
	}
	out := make(map[string]uint16, len(in))
	for key := range in {
		out[key] = 0
	}
	return out
}

func dockerPullProgress(ctx context.Context, ref string, out io.Writer) error {
	meta, err := ensureDockerImage(ref)
	if err != nil {
		return err
	}
	select {
	case <-ctx.Done():
		return ctx.Err()
	default:
	}
	_, err = fmt.Fprintf(out, "{\"status\":\"Pulled\",\"id\":%q}\n", meta.Reference)
	return err
}
