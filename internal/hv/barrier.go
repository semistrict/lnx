//go:build darwin

package hv

// #include <stdint.h>
// static inline void dmb_ishst(void) { __asm__ volatile("dmb ishst" ::: "memory"); }
// static inline void dmb_ish(void)   { __asm__ volatile("dmb ish"   ::: "memory"); }
import "C"

// DmbISHST issues a DMB ISHST (store-store barrier, inner-shareable domain).
// Ensures all preceding stores are visible before subsequent stores.
// This is the ARM64 equivalent of QEMU's smp_wmb().
func DmbISHST() { C.dmb_ishst() }

// DmbISH issues a DMB ISH (full barrier, inner-shareable domain).
// Ensures all preceding loads/stores are visible before subsequent loads/stores.
// This is the ARM64 equivalent of QEMU's smp_mb().
func DmbISH() { C.dmb_ish() }
