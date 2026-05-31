//go:build darwin

package main

import "github.com/semistrict/lnx/internal/macho"

// machoInjectSection replaces the __LNX,__lnxpack section content with blob.
func machoInjectSection(src []byte, blob []byte) ([]byte, error) {
	return macho.InjectSection(src, "__LNX", "__lnxpack", blob)
}

// machoAlignUp rounds size up to the next multiple of align.
func machoAlignUp(size, align uint64) uint64 {
	return macho.AlignUp(size, align)
}
