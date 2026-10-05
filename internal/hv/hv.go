//go:build darwin

// Package hv provides Go bindings for Apple's Hypervisor.framework on ARM64.
// One VM per process. Each vCPU is pinned to its creating goroutine (via
// runtime.LockOSThread).
package hv

// #cgo LDFLAGS: -framework Hypervisor
// #include <Hypervisor/Hypervisor.h>
// #include <mach/mach_time.h>
// #include <signal.h>
// #include <pthread.h>
// #include <string.h>
//
// // dummy_signal is a no-op handler for SIGUSR1. The signal itself
// // interrupts hv_vcpu_run (causing HV_EXIT_REASON_CANCELED) — matching
// // QEMU's hvf_kick_vcpu_thread approach.
// static void dummy_signal(int sig) { (void)sig; }
//
// static void install_kick_signal(void) {
//     struct sigaction sa;
//     memset(&sa, 0, sizeof(sa));
//     sa.sa_handler = dummy_signal;
//     sigaction(SIGUSR1, &sa, NULL);
// }
import "C"
import (
	"fmt"
	"runtime"
	"unsafe"
)

// hvErr converts a Hypervisor.framework return code to a Go error.
func hvErr(ret C.hv_return_t) error {
	if ret == C.HV_SUCCESS {
		return nil
	}
	switch ret {
	case C.HV_ERROR:
		return fmt.Errorf("hv: error (0x%x)", uint32(ret))
	case C.HV_BUSY:
		return fmt.Errorf("hv: busy (0x%x)", uint32(ret))
	case C.HV_BAD_ARGUMENT:
		return fmt.Errorf("hv: bad argument (0x%x)", uint32(ret))
	case C.HV_ILLEGAL_GUEST_STATE:
		return fmt.Errorf("hv: illegal guest state (0x%x)", uint32(ret))
	case C.HV_NO_RESOURCES:
		return fmt.Errorf("hv: no resources (0x%x)", uint32(ret))
	case C.HV_NO_DEVICE:
		return fmt.Errorf("hv: no device (0x%x)", uint32(ret))
	case C.HV_DENIED:
		return fmt.Errorf("hv: denied (0x%x)", uint32(ret))
	case C.HV_UNSUPPORTED:
		return fmt.Errorf("hv: unsupported (0x%x)", uint32(ret))
	default:
		return fmt.Errorf("hv: unknown error (0x%x)", uint32(ret))
	}
}

// ---------- VM ----------

// VMCreate creates a VM for the current process. One VM per process.
func VMCreate() error {
	C.install_kick_signal()
	return hvErr(C.hv_vm_create(nil))
}

// VMDestroy destroys the VM. All vCPUs must be destroyed first.
func VMDestroy() error { return hvErr(C.hv_vm_destroy()) }

// MemFlags are guest physical memory permissions.
type MemFlags = uint64

const (
	MemRead  MemFlags = C.HV_MEMORY_READ
	MemWrite MemFlags = C.HV_MEMORY_WRITE
	MemExec  MemFlags = C.HV_MEMORY_EXEC
	MemRWX            = MemRead | MemWrite | MemExec
	MemRW             = MemRead | MemWrite
)

// VMMap maps host virtual memory into the guest physical address space.
// Both addr and ipa must be page-aligned; size must be a page multiple.
func VMMap(addr unsafe.Pointer, ipa, size uint64, flags MemFlags) error {
	return hvErr(C.hv_vm_map(addr, C.hv_ipa_t(ipa), C.size_t(size), C.hv_memory_flags_t(flags)))
}

// VMUnmap removes a mapping from the guest physical address space.
func VMUnmap(ipa, size uint64) error {
	return hvErr(C.hv_vm_unmap(C.hv_ipa_t(ipa), C.size_t(size)))
}

// ---------- VCPU ----------

// ExitReason classifies why a vCPU exited.
type ExitReason uint32

const (
	ExitCanceled ExitReason = C.HV_EXIT_REASON_CANCELED
	ExitException ExitReason = C.HV_EXIT_REASON_EXCEPTION
	ExitVTimer   ExitReason = C.HV_EXIT_REASON_VTIMER_ACTIVATED
	ExitUnknown  ExitReason = C.HV_EXIT_REASON_UNKNOWN
)

func (r ExitReason) String() string {
	switch r {
	case ExitCanceled:
		return "canceled"
	case ExitException:
		return "exception"
	case ExitVTimer:
		return "vtimer"
	default:
		return fmt.Sprintf("unknown(%d)", uint32(r))
	}
}

// ExitInfo holds information about a vCPU exit.
type ExitInfo struct {
	Reason          ExitReason
	Syndrome        uint64 // ESR_EL2
	VirtualAddress  uint64 // FAR_EL2
	PhysicalAddress uint64 // IPA of faulting access
}

// EC returns the Exception Class from the syndrome (bits [31:26]).
func (e ExitInfo) EC() uint32 { return uint32((e.Syndrome >> 26) & 0x3f) }

// ISS returns the Instruction Specific Syndrome (bits [24:0]).
func (e ExitInfo) ISS() uint32 { return uint32(e.Syndrome & 0x1ffffff) }

// ESR Exception Classes relevant to VMM exit handling.
const (
	ECWFx       uint32 = 0x01 // WFI or WFE
	ECSMC64     uint32 = 0x17 // SMC from AArch64 (PSCI)
	ECSysReg    uint32 = 0x18 // MRS/MSR/SYS trap
	ECDataAbort uint32 = 0x24 // Data Abort from lower EL (MMIO)
)

// Reg identifies an ARM64 general-purpose or special register.
type Reg = C.hv_reg_t

// General-purpose and special registers.
var (
	RegX0   Reg = C.HV_REG_X0
	RegX1   Reg = C.HV_REG_X1
	RegX2   Reg = C.HV_REG_X2
	RegX3   Reg = C.HV_REG_X3
	RegX4   Reg = C.HV_REG_X4
	RegX5   Reg = C.HV_REG_X5
	RegX6   Reg = C.HV_REG_X6
	RegX7   Reg = C.HV_REG_X7
	RegX8   Reg = C.HV_REG_X8
	RegX9   Reg = C.HV_REG_X9
	RegX10  Reg = C.HV_REG_X10
	RegX11  Reg = C.HV_REG_X11
	RegX12  Reg = C.HV_REG_X12
	RegX13  Reg = C.HV_REG_X13
	RegX14  Reg = C.HV_REG_X14
	RegX15  Reg = C.HV_REG_X15
	RegX16  Reg = C.HV_REG_X16
	RegX17  Reg = C.HV_REG_X17
	RegX18  Reg = C.HV_REG_X18
	RegX19  Reg = C.HV_REG_X19
	RegX20  Reg = C.HV_REG_X20
	RegX21  Reg = C.HV_REG_X21
	RegX22  Reg = C.HV_REG_X22
	RegX23  Reg = C.HV_REG_X23
	RegX24  Reg = C.HV_REG_X24
	RegX25  Reg = C.HV_REG_X25
	RegX26  Reg = C.HV_REG_X26
	RegX27  Reg = C.HV_REG_X27
	RegX28  Reg = C.HV_REG_X28
	RegFP   Reg = C.HV_REG_FP
	RegLR   Reg = C.HV_REG_LR
	RegPC   Reg = C.HV_REG_PC
	RegCPSR Reg = C.HV_REG_CPSR
	RegFPCR Reg = C.HV_REG_FPCR
	RegFPSR Reg = C.HV_REG_FPSR
)

// RegXn returns the Reg for Xn (0–30).
func RegXn(n int) Reg { return Reg(C.HV_REG_X0 + C.hv_reg_t(n)) }

// SysReg identifies an ARM64 system register.
type SysReg = C.hv_sys_reg_t

// System registers used by the VMM.
var (
	SysRegSCTLR_EL1   SysReg = C.HV_SYS_REG_SCTLR_EL1
	SysRegCPACR_EL1    SysReg = C.HV_SYS_REG_CPACR_EL1
	SysRegTTBR0_EL1    SysReg = C.HV_SYS_REG_TTBR0_EL1
	SysRegTTBR1_EL1    SysReg = C.HV_SYS_REG_TTBR1_EL1
	SysRegTCR_EL1      SysReg = C.HV_SYS_REG_TCR_EL1
	SysRegMAIR_EL1     SysReg = C.HV_SYS_REG_MAIR_EL1
	SysRegVBAR_EL1     SysReg = C.HV_SYS_REG_VBAR_EL1
	SysRegELR_EL1      SysReg = C.HV_SYS_REG_ELR_EL1
	SysRegSPSR_EL1     SysReg = C.HV_SYS_REG_SPSR_EL1
	SysRegSP_EL0       SysReg = C.HV_SYS_REG_SP_EL0
	SysRegSP_EL1       SysReg = C.HV_SYS_REG_SP_EL1
	SysRegESR_EL1      SysReg = C.HV_SYS_REG_ESR_EL1
	SysRegFAR_EL1      SysReg = C.HV_SYS_REG_FAR_EL1
	SysRegMPIDR_EL1    SysReg = C.HV_SYS_REG_MPIDR_EL1
	SysRegMIDR_EL1     SysReg = C.HV_SYS_REG_MIDR_EL1
	SysRegTPIDR_EL0    SysReg = C.HV_SYS_REG_TPIDR_EL0
	SysRegTPIDRRO_EL0  SysReg = C.HV_SYS_REG_TPIDRRO_EL0
	SysRegTPIDR_EL1    SysReg = C.HV_SYS_REG_TPIDR_EL1
	SysRegCNTV_CTL_EL0 SysReg = C.HV_SYS_REG_CNTV_CTL_EL0
	SysRegCNTV_CVAL_EL0 SysReg = C.HV_SYS_REG_CNTV_CVAL_EL0
	SysRegHCR_EL2      SysReg = C.HV_SYS_REG_HCR_EL2
	SysRegCNTHCTL_EL2  SysReg = C.HV_SYS_REG_CNTHCTL_EL2
	SysRegCNTVOFF_EL2  SysReg = C.HV_SYS_REG_CNTVOFF_EL2
)

// InterruptType selects IRQ or FIQ.
type InterruptType = C.hv_interrupt_type_t

var (
	InterruptIRQ InterruptType = C.HV_INTERRUPT_TYPE_IRQ
	InterruptFIQ InterruptType = C.HV_INTERRUPT_TYPE_FIQ
)

// VCPUOps is the subset of VCPU methods used by VMM handlers (PSCI, MMIO,
// vtimer sync, advancePC). Extracted as an interface so handlers can be
// unit-tested with a fake implementation that doesn't require the HV framework.
type VCPUOps interface {
	GetReg(Reg) (uint64, error)
	SetReg(Reg, uint64) error
	GetSysReg(SysReg) (uint64, error)
	SetSysReg(SysReg, uint64) error
	SetPendingInterrupt(InterruptType, bool) error
	SetVTimerMask(bool) error
	GetVTimerOffset() (uint64, error)
}

// VCPU is a virtual CPU bound to its creating OS thread.
type VCPU struct {
	id     C.hv_vcpu_t
	exit   *C.hv_vcpu_exit_t
	thread C.pthread_t // owning pthread, for signal-based kick
}

// NewVCPU creates a vCPU on the current OS thread. The caller must have
// called runtime.LockOSThread() first — all subsequent VCPU method calls
// must happen on the same thread.
func NewVCPU() (*VCPU, error) {
	runtime.LockOSThread()
	v := &VCPU{thread: C.pthread_self()}
	if err := hvErr(C.hv_vcpu_create(&v.id, &v.exit, nil)); err != nil {
		runtime.UnlockOSThread()
		return nil, err
	}
	return v, nil
}

// Kick sends SIGUSR1 to the vCPU's owning thread. This interrupts a
// blocking hv_vcpu_run, causing HV_EXIT_REASON_CANCELED — matching
// QEMU's hvf_kick_vcpu_thread. Unlike ForceExit (hv_vcpus_exit), the
// signal can only be delivered during hv_vcpu_run (a blocking syscall),
// NOT between SetPendingInterrupt and hv_vcpu_run.
func (v *VCPU) Kick() {
	C.pthread_kill(v.thread, C.SIGUSR1)
}

// Destroy releases the vCPU. Must be called from the owning thread.
func (v *VCPU) Destroy() error {
	err := hvErr(C.hv_vcpu_destroy(v.id))
	runtime.UnlockOSThread()
	return err
}

// ID returns the vCPU identifier.
func (v *VCPU) ID() uint64 { return uint64(v.id) }

// Run executes the vCPU until the next exit. Must be called from the owning thread.
func (v *VCPU) Run() (ExitInfo, error) {
	if err := hvErr(C.hv_vcpu_run(v.id)); err != nil {
		return ExitInfo{}, err
	}
	return ExitInfo{
		Reason:          ExitReason(v.exit.reason),
		Syndrome:        uint64(v.exit.exception.syndrome),
		VirtualAddress:  uint64(v.exit.exception.virtual_address),
		PhysicalAddress: uint64(v.exit.exception.physical_address),
	}, nil
}

// ForceExit cancels a running hv_vcpu_run from another thread.
func (v *VCPU) ForceExit() error {
	id := v.id
	return hvErr(C.hv_vcpus_exit(&id, 1))
}

// GetReg reads a general-purpose or special register.
func (v *VCPU) GetReg(reg Reg) (uint64, error) {
	var val C.uint64_t
	if err := hvErr(C.hv_vcpu_get_reg(v.id, reg, &val)); err != nil {
		return 0, err
	}
	return uint64(val), nil
}

// SetReg writes a general-purpose or special register.
func (v *VCPU) SetReg(reg Reg, val uint64) error {
	return hvErr(C.hv_vcpu_set_reg(v.id, reg, C.uint64_t(val)))
}

// GetSysReg reads a system register.
func (v *VCPU) GetSysReg(reg SysReg) (uint64, error) {
	var val C.uint64_t
	if err := hvErr(C.hv_vcpu_get_sys_reg(v.id, reg, &val)); err != nil {
		return 0, err
	}
	return uint64(val), nil
}

// SetSysReg writes a system register.
func (v *VCPU) SetSysReg(reg SysReg, val uint64) error {
	return hvErr(C.hv_vcpu_set_sys_reg(v.id, reg, C.uint64_t(val)))
}

// SetVTimerOffset sets the virtual timer offset.
// Guest sees: CNTVCT_EL0 = mach_absolute_time() - offset.
func (v *VCPU) SetVTimerOffset(offset uint64) error {
	return hvErr(C.hv_vcpu_set_vtimer_offset(v.id, C.uint64_t(offset)))
}

// GetVTimerOffset returns the virtual timer offset.
func (v *VCPU) GetVTimerOffset() (uint64, error) {
	var off C.uint64_t
	if err := hvErr(C.hv_vcpu_get_vtimer_offset(v.id, &off)); err != nil {
		return 0, err
	}
	return uint64(off), nil
}

// SetVTimerMask controls whether VTimer exits are suppressed.
func (v *VCPU) SetVTimerMask(masked bool) error {
	return hvErr(C.hv_vcpu_set_vtimer_mask(v.id, C.bool(masked)))
}

// MachAbsoluteTime returns the host's mach_absolute_time counter.
// Guest CNTVCT_EL0 = MachAbsoluteTime() - vtimer_offset.
func MachAbsoluteTime() uint64 {
	return uint64(C.mach_absolute_time())
}

// SetPendingInterrupt injects an IRQ or FIQ into the vCPU.
// Cleared automatically after the next hv_vcpu_run returns.
func (v *VCPU) SetPendingInterrupt(t InterruptType, pending bool) error {
	return hvErr(C.hv_vcpu_set_pending_interrupt(v.id, t, C.bool(pending)))
}

// SetTrapDebugExceptions controls whether guest debug exceptions (BRK, etc.)
// are trapped to the VMM.
func (v *VCPU) SetTrapDebugExceptions(trap bool) error {
	return hvErr(C.hv_vcpu_set_trap_debug_exceptions(v.id, C.bool(trap)))
}
