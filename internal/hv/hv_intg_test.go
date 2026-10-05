//go:build darwin && integration

package hv_test

import (
	"encoding/binary"
	"testing"
	"time"
	"unsafe"

	"github.com/semistrict/lnx/internal/hv"
	"golang.org/x/sys/unix"
)

const pageSize = 16384 // Apple Silicon uses 16K pages.

func mapCodePage(t *testing.T, gpa uint64, insns ...uint32) ([]byte, func()) {
	t.Helper()
	mem, err := unix.Mmap(-1, 0, pageSize,
		unix.PROT_READ|unix.PROT_WRITE, unix.MAP_ANON|unix.MAP_PRIVATE)
	if err != nil {
		t.Fatalf("mmap: %v", err)
	}
	for i, insn := range insns {
		binary.LittleEndian.PutUint32(mem[i*4:], insn)
	}
	if err := hv.VMMap(unsafe.Pointer(&mem[0]), gpa, pageSize, hv.MemRWX); err != nil {
		unix.Munmap(mem)
		t.Fatalf("VMMap: %v", err)
	}
	return mem, func() {
		hv.VMUnmap(gpa, pageSize)
		unix.Munmap(mem)
	}
}

func createVCPU(t *testing.T, pc uint64) *hv.VCPU {
	t.Helper()
	vcpu, err := hv.NewVCPU()
	if err != nil {
		t.Fatalf("NewVCPU: %v", err)
	}
	if err := vcpu.SetReg(hv.RegPC, pc); err != nil {
		vcpu.Destroy()
		t.Fatalf("set PC: %v", err)
	}
	// EL1h, DAIF masked.
	if err := vcpu.SetReg(hv.RegCPSR, 0x3C5); err != nil {
		vcpu.Destroy()
		t.Fatalf("set CPSR: %v", err)
	}
	return vcpu
}

// TestVCPU_SMC tests that SMC traps to the VMM by default.
func TestVCPU_SMC(t *testing.T) {
	if err := hv.VMCreate(); err != nil {
		t.Fatalf("VMCreate: %v", err)
	}
	defer hv.VMDestroy()

	const gpa uint64 = 0x4000_0000
	_, cleanup := mapCodePage(t, gpa,
		0xd2800540, // mov x0, #42
		0xd4000003, // smc #0
	)
	defer cleanup()

	vcpu := createVCPU(t, gpa)
	defer vcpu.Destroy()

	exit, err := vcpu.Run()
	if err != nil {
		t.Fatalf("Run: %v", err)
	}

	t.Logf("exit: reason=%v ec=0x%02x iss=0x%x syndrome=0x%x",
		exit.Reason, exit.EC(), exit.ISS(), exit.Syndrome)

	if exit.Reason != hv.ExitException {
		t.Fatalf("expected ExitException, got %v", exit.Reason)
	}
	if exit.EC() != hv.ECSMC64 {
		t.Fatalf("expected EC=0x17 (SMC64), got EC=0x%02x", exit.EC())
	}

	x0, _ := vcpu.GetReg(hv.RegX0)
	if x0 != 42 {
		t.Fatalf("expected X0=42, got %d", x0)
	}
}

// TestVCPU_WFI_blocks tests that WFI blocks hv_vcpu_run and can be canceled
// via ForceExit from another goroutine.
func TestVCPU_WFI_blocks(t *testing.T) {
	if err := hv.VMCreate(); err != nil {
		t.Fatalf("VMCreate: %v", err)
	}
	defer hv.VMDestroy()

	const gpa uint64 = 0x4000_0000
	_, cleanup := mapCodePage(t, gpa,
		0xd2800540, // mov x0, #42
		0xd503207f, // wfi
		0xd4000003, // smc #0 (reachable after wfi wakes)
	)
	defer cleanup()

	// The vCPU must be created and run on the same OS thread.
	// Create it in a dedicated goroutine and use ForceExit from here.
	type result struct {
		exit hv.ExitInfo
		err  error
		x0   uint64
		pc   uint64
	}
	vcpuReady := make(chan *hv.VCPU, 1)
	ch := make(chan result, 1)

	go func() {
		vcpu, err := hv.NewVCPU()
		if err != nil {
			ch <- result{err: err}
			return
		}
		defer vcpu.Destroy()

		vcpu.SetReg(hv.RegPC, gpa)
		vcpu.SetReg(hv.RegCPSR, 0x3C5)

		vcpuReady <- vcpu

		exit, err := vcpu.Run()
		x0, _ := vcpu.GetReg(hv.RegX0)
		pc, _ := vcpu.GetReg(hv.RegPC)
		ch <- result{exit: exit, err: err, x0: x0, pc: pc}
	}()

	// Wait for vCPU to be created, then let it run into WFI.
	vcpu := <-vcpuReady
	time.Sleep(50 * time.Millisecond)

	// Force-cancel from test goroutine (different thread — allowed by API).
	if err := vcpu.ForceExit(); err != nil {
		t.Fatalf("ForceExit: %v", err)
	}

	select {
	case r := <-ch:
		if r.err != nil {
			t.Fatalf("Run: %v", r.err)
		}
		t.Logf("exit: reason=%v ec=0x%02x syndrome=0x%x",
			r.exit.Reason, r.exit.EC(), r.exit.Syndrome)
		t.Logf("x0=%d pc=0x%x", r.x0, r.pc)

		if r.x0 != 42 {
			t.Fatalf("expected X0=42, got %d", r.x0)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("vCPU did not exit within 5s")
	}
}

// TestHCR_EL2_not_accessible documents that Apple manages HCR_EL2 internally.
// WFI, SMC, and IRQ/FIQ trapping are pre-configured and work without VMM access.
func TestHCR_EL2_not_accessible(t *testing.T) {
	if err := hv.VMCreate(); err != nil {
		t.Fatalf("VMCreate: %v", err)
	}
	defer hv.VMDestroy()

	vcpu, err := hv.NewVCPU()
	if err != nil {
		t.Fatalf("NewVCPU: %v", err)
	}
	defer vcpu.Destroy()

	_, err = vcpu.GetSysReg(hv.SysRegHCR_EL2)
	if err == nil {
		t.Fatal("expected HCR_EL2 to be inaccessible, but read succeeded")
	}
	t.Logf("HCR_EL2 not accessible (expected): %v", err)
	t.Log("Apple sets TWI=1, TSC=1, IRQ/FIQ routing internally")
}

// TestVCPU_ProbeExitReasons runs a small instruction sequence to discover
// what exit reasons the framework produces by default.
func TestVCPU_ProbeExitReasons(t *testing.T) {
	if err := hv.VMCreate(); err != nil {
		t.Fatalf("VMCreate: %v", err)
	}
	defer hv.VMDestroy()

	const gpa uint64 = 0x4000_0000
	_, cleanup := mapCodePage(t, gpa,
		0xd2800540, // mov x0, #42
		0xd2800561, // mov x1, #43
		0xd4200000, // brk #0 (debug breakpoint)
	)
	defer cleanup()

	vcpu := createVCPU(t, gpa)
	defer vcpu.Destroy()

	// Enable debug exception trapping so BRK traps to VMM.
	if err := vcpu.SetTrapDebugExceptions(true); err != nil {
		t.Fatalf("SetTrapDebugExceptions: %v", err)
	}

	exit, err := vcpu.Run()
	if err != nil {
		t.Fatalf("Run: %v", err)
	}

	t.Logf("exit: reason=%v ec=0x%02x iss=0x%x syndrome=0x%x",
		exit.Reason, exit.EC(), exit.ISS(), exit.Syndrome)
	t.Logf("  physical_address=0x%x virtual_address=0x%x",
		exit.PhysicalAddress, exit.VirtualAddress)

	x0, _ := vcpu.GetReg(hv.RegX0)
	x1, _ := vcpu.GetReg(hv.RegX1)
	pc, _ := vcpu.GetReg(hv.RegPC)
	t.Logf("x0=%d x1=%d pc=0x%x", x0, x1, pc)
}

// TestVTimerOffset verifies that vtimer offset can be set and read back.
func TestVTimerOffset(t *testing.T) {
	if err := hv.VMCreate(); err != nil {
		t.Fatalf("VMCreate: %v", err)
	}
	defer hv.VMDestroy()

	vcpu, err := hv.NewVCPU()
	if err != nil {
		t.Fatalf("NewVCPU: %v", err)
	}
	defer vcpu.Destroy()

	const offset uint64 = 0xDEAD_BEEF_CAFE_0000
	if err := vcpu.SetVTimerOffset(offset); err != nil {
		t.Fatalf("SetVTimerOffset: %v", err)
	}

	got, err := vcpu.GetVTimerOffset()
	if err != nil {
		t.Fatalf("GetVTimerOffset: %v", err)
	}
	if got != offset {
		t.Fatalf("expected offset 0x%x, got 0x%x", offset, got)
	}
}

// dumpSysRegs logs the values of commonly needed system registers.
func dumpSysRegs(t *testing.T, vcpu *hv.VCPU) {
	t.Helper()
	for _, sr := range []struct {
		name string
		reg  hv.SysReg
	}{
		{"SCTLR_EL1", hv.SysRegSCTLR_EL1},
		{"MPIDR_EL1", hv.SysRegMPIDR_EL1},
		{"CPACR_EL1", hv.SysRegCPACR_EL1},
		{"SP_EL0", hv.SysRegSP_EL0},
		{"SP_EL1", hv.SysRegSP_EL1},
	} {
		val, err := vcpu.GetSysReg(sr.reg)
		if err != nil {
			t.Logf("  %s: error %v", sr.name, err)
		} else {
			t.Logf("  %s: 0x%016x", sr.name, val)
		}
	}
}
