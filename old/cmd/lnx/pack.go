package main

import (
	"path/filepath"

	"github.com/semistrict/lnx/internal/pack"
)

func readPackedConfig() (*pack.Config, error) {
	return pack.ReadConfig("__LNX", "__lnxpack")
}

func readPackedConfigFrom(path string) (*pack.Config, error) {
	return pack.ReadConfigFrom(path, "__lnxpack")
}

func ensurePackedFiles(cfg *pack.Config) (kernelPath, rootfsPath string, err error) {
	cacheDir := filepath.Join(lnxBase(), "packed-cache")
	return pack.EnsureFiles(cfg, cacheDir)
}
