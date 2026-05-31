//go:build darwin

package main

// Enable CGo for this package on macOS so the linker picks up
// lnxpack_section.c and creates the __LNX,__lnxpack segment.

// #include <stdint.h>
import "C"
