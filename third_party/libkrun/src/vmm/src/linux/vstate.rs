// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

#[cfg(target_arch = "aarch64")]
use arch::ArchMemoryInfo;
use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use libc::{c_int, c_void, siginfo_t};
use std::cell::Cell;
use std::fmt::{Display, Formatter};
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
use std::os::fd::AsRawFd;
use std::os::unix::io::RawFd;
use std::os::unix::thread::JoinHandleExt;

#[cfg(target_arch = "x86_64")]
use std::env;
use std::result;
#[cfg(not(test))]
use std::sync::Barrier;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering, fence};
use std::thread;

use serde::{Deserialize, Serialize};

#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
const KVMIO: u8 = 0xAE;

use super::super::{FC_EXIT_CODE_GENERIC_ERROR, FC_EXIT_CODE_OK};

#[cfg(feature = "amd-sev")]
use super::tee::amdsnp::{AmdSnp, Error as SnpError};

#[cfg(feature = "tdx")]
use super::tee::inteltdx::{Error as TdxError, IntelTdx};

#[cfg(feature = "tee")]
use kbs_types::Tee;

#[cfg(feature = "tee")]
use crate::resources::TeeConfig;
use crate::vmm_config::machine_config::CpuFeaturesTemplate;
#[cfg(target_arch = "x86_64")]
use cpuid::{VmSpec, c3, filter_cpuid, t2};
#[cfg(target_arch = "x86_64")]
use kvm_bindings::{
    CpuId, KVM_CLOCK_TSC_STABLE, KVM_IRQCHIP_IOAPIC, KVM_IRQCHIP_PIC_MASTER, KVM_IRQCHIP_PIC_SLAVE,
    KVM_MAX_CPUID_ENTRIES, KVM_MP_STATE_HALTED, MsrList, Msrs, kvm_clock_data, kvm_debugregs,
    kvm_irqchip, kvm_lapic_state, kvm_mp_state, kvm_msr_entry, kvm_pit_state2, kvm_regs, kvm_sregs,
    kvm_vcpu_events, kvm_xcrs, kvm_xsave,
};
use kvm_bindings::{
    KVM_API_VERSION, KVM_CAP_HALT_POLL, KVM_MEM_GUEST_MEMFD, KVM_SYSTEM_EVENT_RESET,
    KVM_SYSTEM_EVENT_SHUTDOWN, kvm_create_guest_memfd, kvm_enable_cap, kvm_userspace_memory_region,
    kvm_userspace_memory_region2,
};
#[cfg(feature = "tee")]
use kvm_bindings::{KVM_CAP_EXIT_HYPERCALL, KVM_MEMORY_EXIT_FLAG_PRIVATE};
#[cfg(not(target_arch = "riscv64"))]
use kvm_bindings::{KVM_MEMORY_ATTRIBUTE_PRIVATE, kvm_memory_attributes};
#[cfg(target_arch = "aarch64")]
use kvm_bindings::{
    KVM_MP_STATE_RUNNABLE, KVM_REG_ARM_CORE, KVM_REG_ARM64, KVM_REG_ARM64_SYSREG,
    KVM_REG_ARM64_SYSREG_CRM_MASK, KVM_REG_ARM64_SYSREG_CRM_SHIFT, KVM_REG_ARM64_SYSREG_CRN_MASK,
    KVM_REG_ARM64_SYSREG_CRN_SHIFT, KVM_REG_ARM64_SYSREG_OP0_MASK, KVM_REG_ARM64_SYSREG_OP0_SHIFT,
    KVM_REG_ARM64_SYSREG_OP1_MASK, KVM_REG_ARM64_SYSREG_OP1_SHIFT, KVM_REG_ARM64_SYSREG_OP2_MASK,
    KVM_REG_ARM64_SYSREG_OP2_SHIFT, KVM_REG_SIZE_MASK, KVM_REG_SIZE_U8, KVM_REG_SIZE_U16,
    KVM_REG_SIZE_U32, KVM_REG_SIZE_U64, KVM_REG_SIZE_U128, KVM_REG_SIZE_U256, KVM_REG_SIZE_U512,
    KVM_REG_SIZE_U1024, KVM_REG_SIZE_U2048, RegList, kvm_mp_state, kvm_regs, kvm_vcpu_events,
    user_fpsimd_state, user_pt_regs,
};
use kvm_ioctls::{Cap::*, *};
use utils::eventfd::EventFd;
use utils::signal::{Killable, register_signal_handler, sigrtmin};
use utils::sm::StateMachine;
#[cfg(feature = "tee")]
use utils::worker_message::{MemoryProperties, WorkerMessage};
use vm_memory::{
    Address, GuestAddress, GuestMemory, GuestMemoryError, GuestMemoryMmap, GuestMemoryRegion,
    GuestRegionMmap,
};

#[cfg(feature = "amd-sev")]
use super::tee::amdsnp::launch as snp;

/// Signal number (SIGRTMIN) used to kick Vcpus.
pub(crate) const VCPU_RTSIG_OFFSET: i32 = 0;

/// Errors associated with the wrappers over KVM ioctls.
#[derive(Debug)]
pub enum Error {
    #[cfg(target_arch = "x86_64")]
    /// A call to cpuid instruction failed.
    CpuId(cpuid::Error),
    /// Unable to create a KVM guest_memfd.
    CreateGuestMemfd(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Error configuring the floating point related registers
    FPUConfiguration(arch::x86_64::regs::Error),
    /// Invalid guest memory configuration.
    GuestMemoryMmap(GuestMemoryError),
    #[cfg(target_arch = "x86_64")]
    /// Retrieving supported guest MSRs fails.
    GuestMSRs(arch::x86_64::msr::Error),
    /// Hyperthreading flag is not initialized.
    HTNotInitialized,
    /// Unable to enable KVM hypercall exits.
    #[cfg(feature = "tee")]
    HypercallExitEnable(kvm_ioctls::Error),
    /// Cannot configure the IRQ.
    Irq(kvm_ioctls::Error),
    /// The host kernel reports an invalid KVM API version.
    KvmApiVersion(i32),
    /// Cannot initialize the KVM context due to missing capabilities.
    KvmCap(kvm_ioctls::Cap),
    /// Cannot initialize the KVM context due to a missing raw capability.
    KvmRawCap(u32),
    /// Cannot persist deterministic clock state after a synthetic timer jump.
    DeterministicClockState(io::Error),
    #[cfg(feature = "amd-sev")]
    /// Cannot read the CPUID entries from KVM.
    KvmCpuId(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Cannot set the local interruption due to bad configuration.
    LocalIntConfiguration(arch::x86_64::interrupts::Error),
    #[cfg(feature = "tee")]
    /// Missing TEE config
    MissingTeeConfig,
    #[cfg(target_arch = "x86_64")]
    /// Error configuring the MSR registers
    MSRSConfiguration(arch::x86_64::msr::Error),
    /// The number of configured slots is bigger than the maximum reported by KVM.
    NotEnoughMemorySlots,
    #[cfg(target_arch = "aarch64")]
    /// Error configuring the general purpose aarch64 registers.
    REGSConfiguration(arch::aarch64::regs::Error),
    #[cfg(target_arch = "riscv64")]
    /// Error configuring the general purpose riscv64 registers.
    REGSConfiguration(arch::riscv64::regs::Error),
    #[cfg(target_arch = "x86_64")]
    /// Error configuring the general purpose registers
    REGSConfiguration(arch::x86_64::regs::Error),
    /// Cannot set memory region attributes.
    SetMemoryAttributes(kvm_ioctls::Error),
    /// Cannot set the memory regions.
    SetUserMemoryRegion(kvm_ioctls::Error),
    /// Error creating memory map for SHM region.
    ShmMmap(io::Error),
    #[cfg(feature = "amd-sev")]
    /// Error initializing the Secure Virtualization Backend (SNP).
    SnpSecVirtInit(SnpError),
    #[cfg(feature = "amd-sev")]
    /// Error preparing the VM for Secure Virtualization (SNP).
    SnpSecVirtPrepare(SnpError),
    #[cfg(feature = "amd-sev")]
    /// Error attesting the Secure VM (SNP).
    SnpSecVirtAttest(SnpError),
    #[cfg(feature = "tdx")]
    /// Error preparing the VM for Trust Domain Extensions (TDX)
    TdxSecVirtPrepare(TdxError),
    #[cfg(feature = "tdx")]
    /// Error initializing vCPU for Trust Domain Extensions (TDX)
    TdxSecVirtInitVcpu,
    #[cfg(feature = "tee")]
    /// The TEE specified is not supported.
    InvalidTee,
    /// Failed to signal Vcpu.
    SignalVcpu(utils::errno::Error),
    #[cfg(target_arch = "x86_64")]
    /// Error configuring the special registers
    SREGSConfiguration(arch::x86_64::regs::Error),
    #[cfg(target_arch = "aarch64")]
    /// Error doing Vcpu Init on Arm.
    VcpuArmInit(kvm_ioctls::Error),
    #[cfg(target_arch = "aarch64")]
    /// Error getting the Vcpu preferred target on Arm.
    VcpuArmPreferredTarget(kvm_ioctls::Error),
    /// vCPU count is not initialized.
    VcpuCountNotInitialized,
    /// Cannot open the VCPU file descriptor.
    VcpuFd(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Failed to get KVM vcpu debug regs.
    VcpuGetDebugRegs(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Failed to get KVM vcpu lapic.
    VcpuGetLapic(kvm_ioctls::Error),
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    /// Failed to get KVM vcpu mp state.
    VcpuGetMpState(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Failed to get KVM vcpu msrs.
    VcpuGetMsrs(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Failed to get KVM vcpu regs.
    VcpuGetRegs(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Failed to get KVM vcpu sregs.
    VcpuGetSregs(kvm_ioctls::Error),
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    /// Failed to get KVM vcpu event.
    VcpuGetVcpuEvents(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Failed to get KVM vcpu xcrs.
    VcpuGetXcrs(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Failed to get KVM vcpu xsave.
    VcpuGetXsave(kvm_ioctls::Error),
    /// Cannot run the VCPUs.
    VcpuRun(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Failed to set KVM vcpu cpuid.
    VcpuSetCpuid(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Failed to set KVM vcpu debug regs.
    VcpuSetDebugRegs(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Failed to set KVM vcpu lapic.
    VcpuSetLapic(kvm_ioctls::Error),
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    /// Failed to set KVM vcpu mp state.
    VcpuSetMpState(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Failed to set KVM vcpu msrs.
    VcpuSetMsrs(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Failed to set KVM vcpu regs.
    VcpuSetRegs(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Failed to set KVM vcpu sregs.
    VcpuSetSregs(kvm_ioctls::Error),
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    /// Failed to set KVM vcpu event.
    VcpuSetVcpuEvents(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Failed to set KVM vcpu xcrs.
    VcpuSetXcrs(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Failed to set KVM vcpu xsave.
    VcpuSetXsave(kvm_ioctls::Error),
    /// Cannot spawn a new vCPU thread.
    VcpuSpawn(io::Error),
    /// Cannot cleanly initialize vcpu TLS.
    VcpuTlsInit,
    /// Vcpu not present in TLS.
    VcpuTlsNotPresent,
    /// Unexpected KVM_RUN exit reason
    VcpuUnhandledKvmExit,
    /// Unsupported KVM_EXIT_HYPERCALL.
    #[cfg(feature = "tee")]
    VcpuUnsupportedHypercall,
    /// Cannot open the VM file descriptor.
    VmFd(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Failed to get KVM vm pit state.
    VmGetPit2(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Failed to get KVM vm clock.
    VmGetClock(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Failed to get KVM vm irqchip.
    VmGetIrqChip(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Failed to set KVM vm pit state.
    VmSetPit2(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Failed to set KVM vm clock.
    VmSetClock(kvm_ioctls::Error),
    #[cfg(target_arch = "x86_64")]
    /// Failed to set KVM vm irqchip.
    VmSetIrqChip(kvm_ioctls::Error),
    /// Cannot configure the microvm.
    VmSetup(kvm_ioctls::Error),
    /// Failed to enable split IRQCHIP in vm
    VmSplitIrqchip(kvm_ioctls::Error),
    /// Failed to set vm APIC bus clock rate (in nanoseconds)
    VmApicBusClockRate(kvm_ioctls::Error),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::Error::*;

        match self {
            #[cfg(target_arch = "x86_64")]
            CpuId(e) => write!(f, "Cpuid error: {e:?}"),
            CreateGuestMemfd(e) => write!(f, "Unable to create KVM guest_memfd: {e:?}"),
            GuestMemoryMmap(e) => write!(f, "Guest memory error: {e:?}"),
            #[cfg(target_arch = "x86_64")]
            GuestMSRs(e) => write!(f, "Retrieving supported guest MSRs fails: {e:?}"),
            HTNotInitialized => write!(f, "Hyperthreading flag is not initialized"),
            #[cfg(feature = "tee")]
            HypercallExitEnable(e) => write!(f, "Unable to enable KVM hypercall exits: {e}"),
            KvmApiVersion(v) => {
                write!(f, "The host kernel reports an invalid KVM API version: {v}")
            }
            KvmCap(cap) => write!(f, "Missing KVM capabilities: {cap:?}"),
            KvmRawCap(cap) => write!(f, "Missing KVM capability: {cap}"),
            DeterministicClockState(e) => write!(
                f,
                "Failed to persist deterministic clock state after timer jump: {e}"
            ),
            #[cfg(feature = "amd-sev")]
            KvmCpuId(e) => write!(f, "Cannot read CPUID entries from KVM: {e}"),
            VcpuCountNotInitialized => write!(f, "vCPU count is not initialized"),
            VmFd(e) => write!(f, "Cannot open the VM file descriptor: {e}"),
            VcpuFd(e) => write!(f, "Cannot open the VCPU file descriptor: {e}"),
            VmSetup(e) => write!(f, "Cannot configure the microvm: {e}"),
            VmSplitIrqchip(e) => write!(f, "Failed to enable split IRQCHIP: {e}"),
            VmApicBusClockRate(e) => write!(
                f,
                "Failed to set vm APIC bus clock rate (in nanoseconds): {e}"
            ),
            VcpuRun(e) => write!(f, "Cannot run the VCPUs: {e}"),
            NotEnoughMemorySlots => write!(
                f,
                "The number of configured slots is bigger than the maximum reported by KVM"
            ),
            #[cfg(target_arch = "x86_64")]
            LocalIntConfiguration(e) => write!(
                f,
                "Cannot set the local interruption due to bad configuration: {e:?}"
            ),
            SetMemoryAttributes(e) => write!(f, "Cannot set memory region attributes: {e}"),
            SetUserMemoryRegion(e) => write!(f, "Cannot set the memory regions: {e}"),
            ShmMmap(e) => write!(f, "Error creating memory map for SHM region: {e}"),
            #[cfg(feature = "amd-sev")]
            SnpSecVirtInit(e) => write!(
                f,
                "Error initializing the Secure Virtualization Backend (SEV): {e:?}"
            ),

            #[cfg(feature = "amd-sev")]
            SnpSecVirtPrepare(e) => write!(
                f,
                "Error preparing the VM for Secure Virtualization (SNP): {e:?}"
            ),

            #[cfg(feature = "amd-sev")]
            SnpSecVirtAttest(e) => write!(f, "Error attesting the Secure VM (SNP): {e:?}"),

            SignalVcpu(e) => write!(f, "Failed to signal Vcpu: {e}"),
            #[cfg(feature = "tdx")]
            TdxSecVirtPrepare(e) => write!(
                f,
                "Error preparing the VM for Trust Domain Extensions (TDX): {e:?}"
            ),
            #[cfg(feature = "tdx")]
            TdxSecVirtInitVcpu => write!(
                f,
                "Error initializing vCPU for Trust Domain Extensions (TDX)"
            ),
            #[cfg(feature = "tee")]
            MissingTeeConfig => write!(f, "Missing TEE configuration"),
            #[cfg(target_arch = "x86_64")]
            MSRSConfiguration(e) => write!(f, "Error configuring the MSR registers: {e:?}"),
            #[cfg(target_arch = "aarch64")]
            REGSConfiguration(e) => write!(
                f,
                "Error configuring the general purpose aarch64 registers: {e:?}"
            ),
            #[cfg(target_arch = "riscv64")]
            REGSConfiguration(e) => write!(
                f,
                "Error configuring the general purpose riscv64 registers: {e:?}"
            ),
            #[cfg(target_arch = "x86_64")]
            REGSConfiguration(e) => {
                write!(f, "Error configuring the general purpose registers: {e:?}")
            }
            #[cfg(target_arch = "x86_64")]
            SREGSConfiguration(e) => write!(f, "Error configuring the special registers: {e:?}"),
            #[cfg(target_arch = "x86_64")]
            FPUConfiguration(e) => write!(
                f,
                "Error configuring the floating point related registers: {e:?}"
            ),
            Irq(e) => write!(f, "Cannot configure the IRQ: {e}"),
            #[cfg(target_arch = "x86_64")]
            VcpuGetDebugRegs(e) => write!(f, "Failed to get KVM vcpu debug regs: {e}"),
            #[cfg(target_arch = "x86_64")]
            VcpuGetLapic(e) => write!(f, "Failed to get KVM vcpu lapic: {e}"),
            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            VcpuGetMpState(e) => write!(f, "Failed to get KVM vcpu mp state: {e}"),
            #[cfg(target_arch = "x86_64")]
            VcpuGetMsrs(e) => write!(f, "Failed to get KVM vcpu msrs: {e}"),
            #[cfg(target_arch = "x86_64")]
            VcpuGetRegs(e) => write!(f, "Failed to get KVM vcpu regs: {e}"),
            #[cfg(target_arch = "x86_64")]
            VcpuGetSregs(e) => write!(f, "Failed to get KVM vcpu sregs: {e}"),
            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            VcpuGetVcpuEvents(e) => write!(f, "Failed to get KVM vcpu event: {e}"),
            #[cfg(target_arch = "x86_64")]
            VcpuGetXcrs(e) => write!(f, "Failed to get KVM vcpu xcrs: {e}"),
            #[cfg(target_arch = "x86_64")]
            VcpuGetXsave(e) => write!(f, "Failed to get KVM vcpu xsave: {e}"),
            #[cfg(target_arch = "x86_64")]
            VcpuSetCpuid(e) => write!(f, "Failed to set KVM vcpu cpuid: {e}"),
            #[cfg(target_arch = "x86_64")]
            VcpuSetDebugRegs(e) => write!(f, "Failed to set KVM vcpu debug regs: {e}"),
            #[cfg(target_arch = "x86_64")]
            VcpuSetLapic(e) => write!(f, "Failed to set KVM vcpu lapic: {e}"),
            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            VcpuSetMpState(e) => write!(f, "Failed to set KVM vcpu mp state: {e}"),
            #[cfg(target_arch = "x86_64")]
            VcpuSetMsrs(e) => write!(f, "Failed to set KVM vcpu msrs: {e}"),
            #[cfg(target_arch = "x86_64")]
            VcpuSetRegs(e) => write!(f, "Failed to set KVM vcpu regs: {e}"),
            #[cfg(target_arch = "x86_64")]
            VcpuSetSregs(e) => write!(f, "Failed to set KVM vcpu sregs: {e}"),
            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            VcpuSetVcpuEvents(e) => write!(f, "Failed to set KVM vcpu event: {e}"),
            #[cfg(target_arch = "x86_64")]
            VcpuSetXcrs(e) => write!(f, "Failed to set KVM vcpu xcrs: {e}"),
            #[cfg(target_arch = "x86_64")]
            VcpuSetXsave(e) => write!(f, "Failed to set KVM vcpu xsave: {e}"),
            VcpuSpawn(e) => write!(f, "Cannot spawn a new vCPU thread: {e}"),
            VcpuTlsInit => write!(f, "Cannot clean init vcpu TLS"),
            VcpuTlsNotPresent => write!(f, "Vcpu not present in TLS"),
            VcpuUnhandledKvmExit => write!(f, "Unexpected KVM_RUN exit reason"),
            #[cfg(feature = "tee")]
            VcpuUnsupportedHypercall => write!(f, "Unsupported KVM_EXIT_HYPERCALL"),
            #[cfg(target_arch = "x86_64")]
            VmGetPit2(e) => write!(f, "Failed to get KVM vm pit state: {e}"),
            #[cfg(target_arch = "x86_64")]
            VmGetClock(e) => write!(f, "Failed to get KVM vm clock: {e}"),
            #[cfg(target_arch = "x86_64")]
            VmGetIrqChip(e) => write!(f, "Failed to get KVM vm irqchip: {e}"),
            #[cfg(target_arch = "x86_64")]
            VmSetPit2(e) => write!(f, "Failed to set KVM vm pit state: {e}"),
            #[cfg(target_arch = "x86_64")]
            VmSetClock(e) => write!(f, "Failed to set KVM vm clock: {e}"),
            #[cfg(target_arch = "x86_64")]
            VmSetIrqChip(e) => write!(f, "Failed to set KVM vm irqchip: {e}"),
            #[cfg(target_arch = "aarch64")]
            VcpuArmPreferredTarget(e) => {
                write!(f, "Error getting the Vcpu preferred target on Arm: {e}")
            }
            #[cfg(target_arch = "aarch64")]
            VcpuArmInit(e) => write!(f, "Error doing Vcpu Init on Arm: {e}"),

            #[cfg(feature = "tee")]
            InvalidTee => write!(f, "TEE selected is not currently supported"),
        }
    }
}

pub type Result<T> = result::Result<T, Error>;

fn deterministic_time_enabled() -> bool {
    std::env::var_os("KRUN_DETERMINISTIC_TIME").is_some_and(|value| value == "1")
}

static DETERMINISTIC_HOST_ACTIVITY: AtomicUsize = AtomicUsize::new(0);
#[cfg(target_arch = "aarch64")]
const KVM_CAP_ARM_WFI_EXIT: u32 = 249;

pub fn deterministic_host_activity_begin() {
    DETERMINISTIC_HOST_ACTIVITY.fetch_add(1, Ordering::AcqRel);
}

pub fn deterministic_host_activity_end() {
    DETERMINISTIC_HOST_ACTIVITY.fetch_sub(1, Ordering::AcqRel);
}

fn deterministic_host_idle() -> bool {
    DETERMINISTIC_HOST_ACTIVITY.load(Ordering::Acquire) == 0
        && polly::event_manager::active_event_dispatches() == 0
}

fn configure_deterministic_vm_clock(kvm: &Kvm, vm_fd: &VmFd) -> Result<()> {
    if !deterministic_time_enabled() {
        return Ok(());
    }

    disable_halt_poll(kvm, vm_fd)?;

    #[cfg(target_arch = "aarch64")]
    enable_arm_wfi_exit(kvm, vm_fd)?;

    #[cfg(target_arch = "x86_64")]
    {
        if !kvm.check_extension(AdjustClock) {
            return Err(Error::KvmCap(AdjustClock));
        }
        vm_fd
            .set_clock(&kvm_clock_data {
                clock: 0,
                flags: 0,
                pad0: 0,
                realtime: 0,
                host_tsc: 0,
                pad: [0; 4],
            })
            .map_err(Error::VmSetClock)?;
    }

    Ok(())
}

#[cfg(target_arch = "aarch64")]
fn enable_arm_wfi_exit(kvm: &Kvm, vm_fd: &VmFd) -> Result<()> {
    if kvm.check_extension_raw(KVM_CAP_ARM_WFI_EXIT as libc::c_ulong) <= 0 {
        return Err(Error::KvmRawCap(KVM_CAP_ARM_WFI_EXIT));
    }
    vm_enable_cap(
        vm_fd,
        &kvm_enable_cap {
            cap: KVM_CAP_ARM_WFI_EXIT,
            flags: 0,
            args: [0, 0, 0, 0],
            ..Default::default()
        },
    )
}

fn disable_halt_poll(kvm: &Kvm, vm_fd: &VmFd) -> Result<()> {
    if kvm.check_extension_raw(KVM_CAP_HALT_POLL as libc::c_ulong) <= 0 {
        return Err(Error::KvmRawCap(KVM_CAP_HALT_POLL));
    }
    vm_enable_cap(
        vm_fd,
        &kvm_enable_cap {
            cap: KVM_CAP_HALT_POLL,
            flags: 0,
            args: [0, 0, 0, 0],
            ..Default::default()
        },
    )
}

#[cfg(target_arch = "x86_64")]
fn vm_enable_cap(vm_fd: &VmFd, cap: &kvm_enable_cap) -> Result<()> {
    vm_fd.enable_cap(cap).map_err(Error::VmSetup)
}

#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
fn vm_enable_cap(vm_fd: &VmFd, cap: &kvm_enable_cap) -> Result<()> {
    let request = nix::request_code_write!(KVMIO, 0xa3, std::mem::size_of::<kvm_enable_cap>());
    let rc = unsafe { libc::ioctl(vm_fd.as_raw_fd(), request, cap) };
    if rc == 0 {
        Ok(())
    } else {
        Err(Error::VmSetup(kvm_ioctls::Error::new(
            std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EINVAL),
        )))
    }
}

#[cfg(target_arch = "x86_64")]
fn configure_deterministic_vcpu_clock(vcpu: &VcpuFd) -> Result<()> {
    if deterministic_time_enabled() {
        vcpu.set_tsc_khz((DETERMINISTIC_X86_TSC_HZ / 1000) as u32)
            .map_err(Error::VcpuFd)?;
    }
    Ok(())
}

fn record_deterministic_timer_jump(deadline_ticks: u64, counter_frequency_hz: u64) -> Result<()> {
    let Some(path) = std::env::var_os("KRUN_DETERMINISTIC_CLOCK_STATE").map(PathBuf::from) else {
        return Ok(());
    };
    update_deterministic_clock_state_file(&path, deadline_ticks, counter_frequency_hz)
        .map_err(Error::DeterministicClockState)?;
    append_deterministic_timer_jump(deadline_ticks, counter_frequency_hz);
    Ok(())
}

fn update_deterministic_clock_state_file(
    path: &Path,
    deadline_ticks: u64,
    counter_frequency_hz: u64,
) -> io::Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let mut fields = parse_clock_state_lines(&raw);
    let recorded_frequency_hz = clock_state_u64(&fields, "counter_frequency_hz").unwrap_or(0);
    let pristine_clock_state = clock_state_u64(&fields, "timer_jump_count").unwrap_or(0) == 0
        && clock_state_u64(&fields, "last_timer_deadline_ticks").unwrap_or(0) == 0
        && clock_state_u64(&fields, "realtime_unix_nanos").unwrap_or(0) == 0
        && clock_state_u64(&fields, "monotonic_nanos").unwrap_or(0) == 0;
    if recorded_frequency_hz != 0
        && recorded_frequency_hz != counter_frequency_hz
        && !pristine_clock_state
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "deterministic counter frequency mismatch: state={recorded_frequency_hz} current={counter_frequency_hz}"
            ),
        ));
    }
    set_clock_state_field(
        &mut fields,
        "counter_frequency_hz",
        counter_frequency_hz.to_string(),
    );
    let deadline_nanos = ticks_to_nanos(deadline_ticks, counter_frequency_hz);
    let realtime_unix_nanos = clock_state_u64(&fields, "realtime_unix_nanos")
        .unwrap_or(0)
        .max(deadline_nanos);
    let monotonic_nanos = clock_state_u64(&fields, "monotonic_nanos")
        .unwrap_or(0)
        .max(deadline_nanos);
    set_clock_state_field(
        &mut fields,
        "realtime_unix_nanos",
        realtime_unix_nanos.to_string(),
    );
    set_clock_state_field(&mut fields, "monotonic_nanos", monotonic_nanos.to_string());
    let jump_count = fields
        .iter()
        .find(|(key, _)| key == "timer_jump_count")
        .and_then(|(_, value)| value.parse::<u64>().ok())
        .unwrap_or(0)
        .saturating_add(1);
    set_clock_state_field(&mut fields, "timer_jump_count", jump_count.to_string());
    set_clock_state_field(
        &mut fields,
        "last_timer_deadline_ticks",
        deadline_ticks.to_string(),
    );
    let mut content = fields
        .into_iter()
        .map(|(key, value)| format!("{key}={value}\n"))
        .collect::<String>();
    if !content.ends_with('\n') {
        content.push('\n');
    }

    let tmp_path = path.with_extension("state.tmp");
    {
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp_path, path)
}

fn clock_state_u64(fields: &[(String, String)], key: &str) -> Option<u64> {
    fields
        .iter()
        .find(|(field, _)| field == key)
        .and_then(|(_, value)| value.parse::<u64>().ok())
}

fn ticks_to_nanos(ticks: u64, counter_frequency_hz: u64) -> u64 {
    if counter_frequency_hz == 0 {
        return ticks;
    }
    let nanos =
        u128::from(ticks).saturating_mul(1_000_000_000u128) / u128::from(counter_frequency_hz);
    nanos.min(u128::from(u64::MAX)) as u64
}

fn append_deterministic_timer_jump(deadline_ticks: u64, counter_frequency_hz: u64) {
    let Some(path) = std::env::var_os("KRUN_DETERMINISTIC_TIMER_JUMPS").map(PathBuf::from) else {
        return;
    };
    let deadline_nanos = ticks_to_nanos(deadline_ticks, counter_frequency_hz);
    let line = format!(
        "deadline_ticks={deadline_ticks} counter_frequency_hz={counter_frequency_hz} deadline_nanos={deadline_nanos}\n"
    );
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| file.write_all(line.as_bytes()));
    if let Err(e) = result {
        warn!(
            "deterministic_time.timer_jump_append_failed path={} error={e}",
            path.display()
        );
    }
}

fn parse_clock_state_lines(raw: &str) -> Vec<(String, String)> {
    raw.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn set_clock_state_field(fields: &mut Vec<(String, String)>, key: &str, value: String) {
    if let Some((_, existing)) = fields.iter_mut().find(|(field, _)| field == key) {
        *existing = value;
    } else {
        fields.push((key.to_string(), value));
    }
}

#[cfg(feature = "tee")]
#[derive(Debug)]
pub struct MeasuredRegion {
    pub guest_addr: u64,
    pub host_addr: u64,
    pub size: usize,
}

/// Describes a KVM context that gets attached to the microVM.
/// It gives access to the functionality of the KVM wrapper as
/// long as every required KVM capability is present on the host.
pub struct KvmContext {
    kvm: Kvm,
    max_memslots: usize,
}

impl KvmContext {
    pub fn new() -> Result<Self> {
        let kvm = Kvm::new().map_err(Error::VmFd)?;

        // Check that KVM has the correct version.
        if kvm.get_api_version() != KVM_API_VERSION as i32 {
            return Err(Error::KvmApiVersion(kvm.get_api_version()));
        }

        // A list of KVM capabilities we want to check.
        #[cfg(target_arch = "x86_64")]
        let capabilities = [Irqchip, Ioeventfd, Irqfd, UserMemory, SetTssAddr];

        #[cfg(target_arch = "aarch64")]
        let capabilities = [Irqchip, Ioeventfd, Irqfd, UserMemory, ArmPsci02];

        #[cfg(target_arch = "riscv64")]
        let capabilities = [Irqchip, Ioeventfd, Irqfd, UserMemory];

        // Check that all desired capabilities are supported.
        match capabilities
            .iter()
            .find(|&capability| !kvm.check_extension(*capability))
        {
            None => {
                let max_memslots = kvm.get_nr_memslots();
                Ok(KvmContext { kvm, max_memslots })
            }

            Some(c) => Err(Error::KvmCap(*c)),
        }
    }

    pub fn fd(&self) -> &Kvm {
        &self.kvm
    }

    /// Get the maximum number of memory slots reported by this KVM context.
    pub fn max_memslots(&self) -> usize {
        self.max_memslots
    }
}

/// A wrapper around creating and using a VM.
pub struct Vm {
    fd: VmFd,
    next_mem_slot: u32,

    // X86 specific fields.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    supported_cpuid: CpuId,
    #[cfg(target_arch = "x86_64")]
    supported_msrs: MsrList,

    #[cfg(feature = "amd-sev")]
    tee: Option<AmdSnp>,

    #[cfg(feature = "tdx")]
    tdx: Option<IntelTdx>,

    #[cfg(feature = "tee")]
    pub tee_config: Tee,

    pub guest_memfds: Vec<(Range<u64>, RawFd)>,
}

impl Vm {
    /// Constructs a new `Vm` using the given `Kvm` instance.
    #[cfg(not(feature = "tee"))]
    pub fn new(kvm: &Kvm) -> Result<Self> {
        //create fd for interacting with kvm-vm specific functions
        let vm_fd = kvm.create_vm().map_err(Error::VmFd)?;
        configure_deterministic_vm_clock(kvm, &vm_fd)?;

        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        let supported_cpuid = kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .map_err(Error::VmFd)?;
        #[cfg(target_arch = "x86_64")]
        let supported_msrs =
            arch::x86_64::msr::supported_guest_msrs(kvm).map_err(Error::GuestMSRs)?;

        Ok(Vm {
            fd: vm_fd,
            next_mem_slot: 0,
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            supported_cpuid,
            #[cfg(target_arch = "x86_64")]
            supported_msrs,
            guest_memfds: Vec::new(),
        })
    }

    #[cfg(feature = "amd-sev")]
    pub fn new(kvm: &Kvm, tee_config: &TeeConfig) -> Result<Self> {
        //create fd for interacting with kvm-vm specific functions
        let vm_fd = kvm
            .create_vm_with_type(4 /* KVM_X86_SNP_VM */)
            .map_err(Error::VmFd)?;
        configure_deterministic_vm_clock(kvm, &vm_fd)?;

        let supported_cpuid = kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .map_err(Error::VmFd)?;

        let supported_msrs =
            arch::x86_64::msr::supported_guest_msrs(kvm).map_err(Error::GuestMSRs)?;

        let cap = kvm_enable_cap {
            cap: KVM_CAP_EXIT_HYPERCALL,
            flags: 0,
            args: [1 << 12 /* KVM_HC_MAP_GPA_RANGE */, 0, 0, 0],
            ..Default::default()
        };

        vm_fd.enable_cap(&cap).map_err(Error::HypercallExitEnable)?;

        let tee = match tee_config.tee {
            Tee::Snp => Some(AmdSnp::new().map_err(Error::SnpSecVirtInit)?),
            _ => return Err(Error::InvalidTee),
        };

        Ok(Vm {
            fd: vm_fd,
            next_mem_slot: 0,
            supported_cpuid,
            supported_msrs,
            tee,
            tee_config: tee_config.tee,
            guest_memfds: Vec::new(),
        })
    }

    #[cfg(feature = "tdx")]
    pub fn new(
        kvm: &Kvm,
        tee_config: &TeeConfig,
        _sender: crossbeam_channel::Sender<WorkerMessage>,
    ) -> Result<Self> {
        // create fd for interacting with kvm-vm specific functions
        let vm_fd = kvm
            .create_vm_with_type(tdx::launch::KVM_X86_TDX_VM)
            .map_err(Error::VmFd)?;
        configure_deterministic_vm_clock(kvm, &vm_fd)?;

        let supported_cpuid = kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .map_err(Error::VmFd)?;

        let supported_msrs =
            arch::x86_64::msr::supported_guest_msrs(kvm).map_err(Error::GuestMSRs)?;

        let mut cap = kvm_enable_cap {
            cap: KVM_CAP_EXIT_HYPERCALL,
            flags: 0,
            args: [1 << 12 /* KVM_HC_MAP_GPA_RANGE */, 0, 0, 0],
            ..Default::default()
        };
        vm_fd.enable_cap(&cap).map_err(Error::HypercallExitEnable)?;

        cap.cap = kvm_bindings::KVM_CAP_SPLIT_IRQCHIP;
        cap.args[0] = 24;
        vm_fd.enable_cap(&cap).map_err(Error::VmSplitIrqchip)?;

        cap.cap = 237; // KVM_CAP_X86_APIC_BUS_CYCLES_NS
        cap.args[0] = 40;
        vm_fd.enable_cap(&cap).map_err(Error::VmApicBusClockRate)?;

        Ok(Vm {
            fd: vm_fd,
            next_mem_slot: 0,
            supported_cpuid,
            supported_msrs,
            tdx: Some(IntelTdx::new()),
            tee_config: tee_config.tee,
            guest_memfds: Vec::new(),
        })
    }

    /// Returns a ref to the supported `CpuId` for this Vm.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    pub fn supported_cpuid(&self) -> &CpuId {
        &self.supported_cpuid
    }

    /// Returns a ref to the supported `MsrList` for this Vm.
    #[cfg(target_arch = "x86_64")]
    pub fn supported_msrs(&self) -> &MsrList {
        &self.supported_msrs
    }

    /// Initializes the guest memory.
    pub fn memory_init(
        &mut self,
        guest_mem: &GuestMemoryMmap,
        kvm_max_memslots: usize,
    ) -> Result<()> {
        if guest_mem.num_regions() > kvm_max_memslots {
            return Err(Error::NotEnoughMemorySlots);
        }

        for region in guest_mem.iter() {
            self.memory_region_set(guest_mem, region)?;
        }

        #[cfg(target_arch = "x86_64")]
        self.fd
            .set_tss_address(arch::x86_64::layout::KVM_TSS_ADDRESS as usize)
            .map_err(Error::VmSetup)?;

        Ok(())
    }

    pub fn guest_memfd_get(&self, gpa: u64) -> Option<(RawFd, u64)> {
        for (range, rawfd) in self.guest_memfds.iter() {
            if range.contains(&gpa) {
                return Some((*rawfd, range.start));
            }
        }
        None
    }

    #[allow(unused_mut)]
    fn memory_region_set(
        &mut self,
        guest_mem: &GuestMemoryMmap,
        region: &GuestRegionMmap,
    ) -> Result<()> {
        let host_addr = guest_mem.get_host_address(region.start_addr()).unwrap();
        let start = region.start_addr().raw_value();
        let end = start + region.len();

        // GuestMemfd is generally intended for either of two purposes:
        // * sharing the memory with out-of-process components, and conversely,
        // * hiding the memory completely from the VMM process (Confidential Computing).
        //
        // We only use it for the second use case currently, so don't even try to use it
        // outside of TEE builds. Software-protected VMs are only available on x86_64 and
        // are marked with strongly-worded warnings about them being for development only,
        // as of late 2025. Also, on other architectures like aarch64, guest_memfd in
        // general is unstable for now, so don't try to use it without a reason.

        if cfg!(not(feature = "tee")) {
            let memory_region = kvm_userspace_memory_region {
                slot: self.next_mem_slot,
                guest_phys_addr: start,
                memory_size: region.len(),
                userspace_addr: host_addr as u64,
                flags: 0,
            };

            // Safe because we mapped the memory region, we made sure that the regions
            // are not overlapping.
            unsafe {
                self.fd
                    .set_user_memory_region(memory_region)
                    .map_err(Error::SetUserMemoryRegion)?;
            };
        } else {
            if !self.fd.check_extension(GuestMemfd) {
                return Err(Error::KvmCap(GuestMemfd));
            }

            // Create a guest_memfd and set the region.
            let guest_memfd = self
                .fd
                .create_guest_memfd(kvm_create_guest_memfd {
                    size: region.size() as u64,
                    flags: 0,
                    reserved: [0; 6],
                })
                .map_err(Error::CreateGuestMemfd)?;

            let memory_region = kvm_userspace_memory_region2 {
                slot: self.next_mem_slot,
                flags: KVM_MEM_GUEST_MEMFD,
                guest_phys_addr: start,
                memory_size: region.len(),
                userspace_addr: host_addr as u64,
                guest_memfd_offset: 0,
                guest_memfd: guest_memfd as u32,
                pad1: 0,
                pad2: [0; 14],
            };

            // Safe because we mapped the memory region, we made sure that the regions
            // are not overlapping.
            unsafe {
                self.fd
                    .set_user_memory_region2(memory_region)
                    .map_err(Error::SetUserMemoryRegion)?;
            };

            let attr = kvm_memory_attributes {
                address: start,
                size: region.len(),
                attributes: KVM_MEMORY_ATTRIBUTE_PRIVATE as u64,
                flags: 0,
            };

            self.fd
                .set_memory_attributes(attr)
                .map_err(Error::SetMemoryAttributes)?;

            self.guest_memfds.push((Range { start, end }, guest_memfd));
        }

        self.next_mem_slot += 1;

        Ok(())
    }

    #[cfg(feature = "tdx")]
    pub fn tdx_secure_virt_prepare(&self) -> Result<tdx::launch::Launcher> {
        match &self.tdx {
            Some(t) => t
                .vm_prepare(&self.fd, self.supported_cpuid.clone())
                .map_err(Error::TdxSecVirtPrepare),
            None => Err(Error::InvalidTee),
        }
    }

    #[cfg(feature = "tdx")]
    pub fn tdx_secure_virt_init_vcpus(&self, launcher: &mut tdx::launch::Launcher) -> Result<()> {
        match &self.tdx {
            Some(_) => {
                launcher.init_vcpus(0).unwrap();
                Ok(())
            }
            None => Err(Error::InvalidTee),
        }
    }

    #[cfg(feature = "tdx")]
    pub fn tdx_secure_virt_prepare_memory(
        &self,
        launcher: &mut tdx::launch::Launcher,
        regions: &Vec<crate::vstate::MeasuredRegion>,
    ) -> Result<()> {
        match &self.tdx {
            Some(t) => t
                .configure_td_memory(launcher, regions)
                .map_err(Error::TdxSecVirtPrepare),
            None => Err(Error::InvalidTee),
        }
    }

    #[cfg(feature = "tdx")]
    pub fn tdx_secure_virt_finalize_vm(&self, launcher: tdx::launch::Launcher) -> Result<()> {
        match &self.tdx {
            Some(t) => t.finalize_vm(launcher).map_err(Error::TdxSecVirtPrepare),
            None => Err(Error::InvalidTee),
        }
    }

    #[cfg(feature = "amd-sev")]
    pub fn snp_secure_virt_prepare(
        &self,
        guest_mem: &GuestMemoryMmap,
    ) -> Result<snp::Launcher<snp::Started, RawFd, RawFd>> {
        match &self.tee {
            Some(s) => s
                .vm_prepare(&self.fd, guest_mem)
                .map_err(Error::SnpSecVirtPrepare),
            None => Err(Error::InvalidTee),
        }
    }

    #[cfg(feature = "amd-sev")]
    pub fn snp_secure_virt_measure(
        &self,
        cpuid: CpuId,
        guest_mem: &GuestMemoryMmap,
        measured_regions: Vec<MeasuredRegion>,
        launcher: snp::Launcher<snp::Started, RawFd, RawFd>,
    ) -> Result<()> {
        match &self.tee {
            Some(s) => s
                .vm_measure(cpuid, guest_mem, measured_regions, launcher)
                .map_err(Error::SnpSecVirtAttest),
            None => Err(Error::InvalidTee),
        }
    }

    /// Gets a reference to the kvm file descriptor owned by this VM.
    pub fn fd(&self) -> &VmFd {
        &self.fd
    }

    #[allow(unused)]
    #[cfg(target_arch = "x86_64")]
    /// Saves and returns the Kvm Vm state.
    pub fn save_state(&self) -> Result<VmState> {
        let pitstate = self.fd.get_pit2().map_err(Error::VmGetPit2)?;

        let mut clock = self.fd.get_clock().map_err(Error::VmGetClock)?;
        // This bit is not accepted in SET_CLOCK, clear it.
        clock.flags &= !KVM_CLOCK_TSC_STABLE;

        let mut pic_master = kvm_irqchip {
            chip_id: KVM_IRQCHIP_PIC_MASTER,
            ..Default::default()
        };
        self.fd
            .get_irqchip(&mut pic_master)
            .map_err(Error::VmGetIrqChip)?;

        let mut pic_slave = kvm_irqchip {
            chip_id: KVM_IRQCHIP_PIC_SLAVE,
            ..Default::default()
        };
        self.fd
            .get_irqchip(&mut pic_slave)
            .map_err(Error::VmGetIrqChip)?;

        let mut ioapic = kvm_irqchip {
            chip_id: KVM_IRQCHIP_IOAPIC,
            ..Default::default()
        };
        self.fd
            .get_irqchip(&mut ioapic)
            .map_err(Error::VmGetIrqChip)?;

        Ok(VmState {
            pitstate,
            clock,
            pic_master,
            pic_slave,
            ioapic,
        })
    }

    #[allow(unused)]
    #[cfg(target_arch = "x86_64")]
    /// Restores the Kvm Vm state.
    pub fn restore_state(&self, state: &VmState) -> Result<()> {
        self.fd
            .set_pit2(&state.pitstate)
            .map_err(Error::VmSetPit2)?;
        self.fd.set_clock(&state.clock).map_err(Error::VmSetClock)?;
        self.fd
            .set_irqchip(&state.pic_master)
            .map_err(Error::VmSetIrqChip)?;
        self.fd
            .set_irqchip(&state.pic_slave)
            .map_err(Error::VmSetIrqChip)?;
        self.fd
            .set_irqchip(&state.ioapic)
            .map_err(Error::VmSetIrqChip)?;
        Ok(())
    }
}

#[allow(unused)]
#[cfg(target_arch = "x86_64")]
/// Structure holding VM kvm state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VmState {
    pitstate: kvm_pit_state2,
    clock: kvm_clock_data,
    pic_master: kvm_irqchip,
    pic_slave: kvm_irqchip,
    ioapic: kvm_irqchip,
}

/// Encapsulates configuration parameters for the guest vCPUS.
#[derive(Debug, Eq, PartialEq)]
pub struct VcpuConfig {
    /// Number of guest VCPUs.
    pub vcpu_count: u8,
    /// Enable hyperthreading in the CPUID configuration.
    pub ht_enabled: bool,
    /// CPUID template to use.
    pub cpu_template: Option<CpuFeaturesTemplate>,
    /// Enable nested virtualization in the CPUID configuration.
    pub nested_enabled: bool,
}

// Using this for easier explicit type-casting to help IDEs interpret the code.
type VcpuCell = Cell<Option<*mut Vcpu>>;

/// A wrapper around creating and using a kvm-based VCPU.
pub struct Vcpu {
    fd: VcpuFd,
    id: u8,
    mmio_bus: Option<devices::Bus>,
    #[allow(dead_code)]
    #[cfg_attr(all(test, target_arch = "aarch64"), allow(unused))]
    exit_evt: EventFd,

    #[cfg(target_arch = "x86_64")]
    io_bus: devices::Bus,
    #[cfg(target_arch = "x86_64")]
    cpuid: CpuId,
    #[cfg(target_arch = "x86_64")]
    msr_list: MsrList,
    #[cfg(target_arch = "x86_64")]
    kernel_enomem_workaround: bool,

    #[cfg(target_arch = "aarch64")]
    mpidr: u64,
    #[cfg(target_arch = "aarch64")]
    pending_hvf_timer_restore: Option<HvfTimerRestoreState>,
    #[cfg(target_arch = "aarch64")]
    restore_debug_exits_remaining: u32,

    // The receiving end of events channel owned by the vcpu side.
    event_receiver: Receiver<VcpuEvent>,
    // The transmitting end of the events channel which will be given to the handler.
    event_sender: Option<Sender<VcpuEvent>>,
    // The receiving end of the responses channel which will be given to the handler.
    response_receiver: Option<Receiver<VcpuResponse>>,
    // The transmitting end of the responses channel owned by the vcpu side.
    response_sender: Sender<VcpuResponse>,

    #[cfg(feature = "tee")]
    pm_sender: Sender<WorkerMessage>,
}

impl Vcpu {
    thread_local!(static TLS_VCPU_PTR: VcpuCell = const { Cell::new(None) });

    /// Associates `self` with the current thread.
    ///
    /// It is a prerequisite to successfully run `init_thread_local_data()` before using
    /// `run_on_thread_local()` on the current thread.
    /// This function will return an error if there already is a `Vcpu` present in the TLS.
    fn init_thread_local_data(&mut self) -> Result<()> {
        Self::TLS_VCPU_PTR.with(|cell: &VcpuCell| {
            if cell.get().is_some() {
                return Err(Error::VcpuTlsInit);
            }
            cell.set(Some(self as *mut Vcpu));
            Ok(())
        })
    }

    /// Deassociates `self` from the current thread.
    ///
    /// Should be called if the current `self` had called `init_thread_local_data()` and
    /// now needs to move to a different thread.
    ///
    /// Fails if `self` was not previously associated with the current thread.
    fn reset_thread_local_data(&mut self) -> Result<()> {
        // Best-effort to clean up TLS. If the `Vcpu` was moved to another thread
        // _before_ running this, then there is nothing we can do.
        Self::TLS_VCPU_PTR.with(|cell: &VcpuCell| {
            if let Some(vcpu_ptr) = cell.get()
                && std::ptr::eq(vcpu_ptr, self)
            {
                Self::TLS_VCPU_PTR.with(|cell: &VcpuCell| cell.take());
                return Ok(());
            }
            Err(Error::VcpuTlsNotPresent)
        })
    }

    /// Runs `func` for the `Vcpu` associated with the current thread.
    ///
    /// It requires that `init_thread_local_data()` was run on this thread.
    ///
    /// Fails if there is no `Vcpu` associated with the current thread.
    ///
    /// # Safety
    ///
    /// This is marked unsafe as it allows temporary aliasing through
    /// dereferencing from pointer an already borrowed `Vcpu`.
    unsafe fn run_on_thread_local<F>(func: F) -> Result<()>
    where
        F: FnOnce(&mut Vcpu),
    {
        unsafe {
            Self::TLS_VCPU_PTR.with(|cell: &VcpuCell| {
                if let Some(vcpu_ptr) = cell.get() {
                    // Dereferencing here is safe since `TLS_VCPU_PTR` is populated/non-empty,
                    // and it is being cleared on `Vcpu::drop` so there is no dangling pointer.
                    let vcpu_ref: &mut Vcpu = &mut *vcpu_ptr;
                    func(vcpu_ref);
                    Ok(())
                } else {
                    Err(Error::VcpuTlsNotPresent)
                }
            })
        }
    }

    /// Registers a signal handler which makes use of TLS and kvm immediate exit to
    /// kick the vcpu running on the current thread, if there is one.
    pub fn register_kick_signal_handler() {
        extern "C" fn handle_signal(_: c_int, _: *mut siginfo_t, _: *mut c_void) {
            // This is safe because it's temporarily aliasing the `Vcpu` object, but we are
            // only reading `vcpu.fd` which does not change for the lifetime of the `Vcpu`.
            unsafe {
                let _ = Vcpu::run_on_thread_local(|vcpu: &mut Vcpu| {
                    vcpu.fd.set_kvm_immediate_exit(1);
                    fence(Ordering::Release);
                });
            }
        }

        register_signal_handler(sigrtmin() + VCPU_RTSIG_OFFSET, handle_signal)
            .expect("Failed to register vcpu signal handler");
    }

    /// Constructs a new VCPU for `vm`.
    ///
    /// # Arguments
    ///
    /// * `id` - Represents the CPU number between [0, max vcpus).
    /// * `vm_fd` - The kvm `VmFd` for the virtual machine this vcpu will get attached to.
    /// * `cpuid` - The `CpuId` listing the supported capabilities of this vcpu.
    /// * `msr_list` - The `MsrList` listing the supported MSRs for this vcpu.
    /// * `io_bus` - The io-bus used to access port-io devices.
    /// * `exit_evt` - An `EventFd` that will be written into when this vcpu exits.
    #[cfg(target_arch = "x86_64")]
    pub fn new_x86_64(
        id: u8,
        vm_fd: &VmFd,
        cpuid: CpuId,
        msr_list: MsrList,
        io_bus: devices::Bus,
        exit_evt: EventFd,
        #[cfg(feature = "tee")] pm_sender: Sender<WorkerMessage>,
    ) -> Result<Self> {
        let kvm_vcpu = vm_fd.create_vcpu(id as u64).map_err(Error::VcpuFd)?;
        let (event_sender, event_receiver) = unbounded();
        let (response_sender, response_receiver) = unbounded();

        let kernel_enomem_workaround = if env::var_os("KRUN_ENOMEM_WORKAROUND").is_some() {
            debug!("Enabling ENOMEM workaround");
            true
        } else {
            false
        };

        // Initially the cpuid per vCPU is the one supported by this VM.
        Ok(Vcpu {
            fd: kvm_vcpu,
            id,
            mmio_bus: None,
            exit_evt,
            io_bus,
            cpuid,
            msr_list,
            kernel_enomem_workaround,
            event_receiver,
            event_sender: Some(event_sender),
            response_receiver: Some(response_receiver),
            response_sender,
            #[cfg(feature = "tee")]
            pm_sender,
        })
    }

    /// Constructs a new VCPU for `vm`.
    ///
    /// # Arguments
    ///
    /// * `id` - Represents the CPU number between [0, max vcpus).
    /// * `vm_fd` - The kvm `VmFd` for the virtual machine this vcpu will get attached to.
    /// * `exit_evt` - An `EventFd` that will be written into when this vcpu exits.
    /// * `create_ts` - A timestamp used by the vcpu to calculate its lifetime.
    #[cfg(target_arch = "aarch64")]
    pub fn new_aarch64(id: u8, vm_fd: &VmFd, exit_evt: EventFd) -> Result<Self> {
        let kvm_vcpu = vm_fd.create_vcpu(id as u64).map_err(Error::VcpuFd)?;
        let (event_sender, event_receiver) = unbounded();
        let (response_sender, response_receiver) = unbounded();

        Ok(Vcpu {
            fd: kvm_vcpu,
            id,
            mmio_bus: None,
            exit_evt,
            mpidr: 0,
            pending_hvf_timer_restore: None,
            restore_debug_exits_remaining: 0,
            event_receiver,
            event_sender: Some(event_sender),
            response_receiver: Some(response_receiver),
            response_sender,
        })
    }

    /// Constructs a new VCPU for `vm`.
    ///
    /// # Arguments
    ///
    /// * `id` - Represents the CPU number between [0, max vcpus).
    /// * `vm_fd` - The kvm `VmFd` for the virtual machine this vcpu will get attached to.
    /// * `exit_evt` - An `EventFd` that will be written into when this vcpu exits.
    /// * `create_ts` - A timestamp used by the vcpu to calculate its lifetime.
    #[cfg(target_arch = "riscv64")]
    pub fn new_riscv64(id: u8, vm_fd: &VmFd, exit_evt: EventFd) -> Result<Self> {
        let kvm_vcpu = vm_fd.create_vcpu(id as u64).map_err(Error::VcpuFd)?;
        let (event_sender, event_receiver) = unbounded();
        let (response_sender, response_receiver) = unbounded();

        Ok(Vcpu {
            fd: kvm_vcpu,
            id,
            mmio_bus: None,
            exit_evt,
            event_receiver,
            event_sender: Some(event_sender),
            response_receiver: Some(response_receiver),
            response_sender,
        })
    }

    /// Returns the cpu index as seen by the guest OS.
    pub fn cpu_index(&self) -> u8 {
        self.id
    }

    /// Gets the MPIDR register value.
    #[cfg(target_arch = "aarch64")]
    pub fn get_mpidr(&self) -> u64 {
        self.mpidr
    }

    /// Sets a MMIO bus for this vcpu.
    pub fn set_mmio_bus(&mut self, mmio_bus: devices::Bus) {
        self.mmio_bus = Some(mmio_bus);
    }

    #[cfg(target_arch = "x86_64")]
    #[allow(unused_variables)]
    /// Configures a x86_64 specific vcpu and should be called once per vcpu.
    ///
    /// # Arguments
    ///
    /// * `machine_config` - The machine configuration of this microvm needed for the CPUID configuration.
    /// * `guest_mem` - The guest memory used by this microvm.
    /// * `kernel_start_addr` - Offset from `guest_mem` at which the kernel starts.
    pub fn configure_x86_64(
        &mut self,
        guest_mem: &GuestMemoryMmap,
        kernel_start_addr: GuestAddress,
        vcpu_config: &VcpuConfig,
        kernel_boot: bool,
        pvh: bool,
    ) -> Result<()> {
        let cpuid_vm_spec = VmSpec::new(
            self.id,
            vcpu_config.vcpu_count,
            vcpu_config.ht_enabled,
            vcpu_config.nested_enabled,
        )
        .map_err(Error::CpuId)?;

        filter_cpuid(&mut self.cpuid, &cpuid_vm_spec).map_err(|e| {
            error!("Failure in configuring CPUID for vcpu {}: {:?}", self.id, e);
            Error::CpuId(e)
        })?;

        if let Some(template) = vcpu_config.cpu_template {
            match template {
                CpuFeaturesTemplate::T2 => {
                    t2::set_cpuid_entries(&mut self.cpuid, &cpuid_vm_spec).map_err(Error::CpuId)?
                }
                CpuFeaturesTemplate::C3 => {
                    c3::set_cpuid_entries(&mut self.cpuid, &cpuid_vm_spec).map_err(Error::CpuId)?
                }
            }
        }

        self.fd
            .set_cpuid2(&self.cpuid)
            .map_err(Error::VcpuSetCpuid)?;

        if kernel_boot {
            configure_deterministic_vcpu_clock(&self.fd)?;
            arch::x86_64::msr::setup_msrs(&self.fd).map_err(Error::MSRSConfiguration)?;
            arch::x86_64::regs::setup_regs(&self.fd, kernel_start_addr.raw_value(), self.id, pvh)
                .map_err(Error::REGSConfiguration)?;
            arch::x86_64::regs::setup_fpu(&self.fd).map_err(Error::FPUConfiguration)?;
            arch::x86_64::regs::setup_sregs(guest_mem, &self.fd, self.id, pvh)
                .map_err(Error::SREGSConfiguration)?;
            arch::x86_64::interrupts::set_lint(&self.fd).map_err(Error::LocalIntConfiguration)?;
        }
        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    /// Configures an aarch64 specific vcpu.
    ///
    /// # Arguments
    ///
    /// * `vm_fd` - The kvm `VmFd` for this microvm.
    /// * `guest_mem` - The guest memory used by this microvm.
    /// * `kernel_load_addr` - Offset from `guest_mem` at which the kernel is loaded.
    pub fn configure_aarch64(
        &mut self,
        vm_fd: &VmFd,
        mem_info: &ArchMemoryInfo,
        kernel_load_addr: GuestAddress,
    ) -> Result<()> {
        let mut kvi: kvm_bindings::kvm_vcpu_init = kvm_bindings::kvm_vcpu_init::default();

        // This reads back the kernel's preferred target type.
        vm_fd
            .get_preferred_target(&mut kvi)
            .map_err(Error::VcpuArmPreferredTarget)?;
        // We already checked that the capability is supported.
        kvi.features[0] |= 1 << kvm_bindings::KVM_ARM_VCPU_PSCI_0_2;
        // Non-boot cpus are powered off initially.
        if self.id > 0 {
            kvi.features[0] |= 1 << kvm_bindings::KVM_ARM_VCPU_POWER_OFF;
        }

        if vm_fd.check_extension(kvm_ioctls::Cap::ArmPtrAuthAddress) {
            kvi.features[0] |= 1 << kvm_bindings::KVM_ARM_VCPU_PTRAUTH_ADDRESS;
        }
        if vm_fd.check_extension(kvm_ioctls::Cap::ArmPtrAuthGeneric) {
            kvi.features[0] |= 1 << kvm_bindings::KVM_ARM_VCPU_PTRAUTH_GENERIC;
        }

        self.fd.vcpu_init(&kvi).map_err(Error::VcpuArmInit)?;
        arch::aarch64::regs::setup_regs(&self.fd, self.id, kernel_load_addr.raw_value(), mem_info)
            .map_err(Error::REGSConfiguration)?;

        self.mpidr = arch::aarch64::regs::read_mpidr(&self.fd).map_err(Error::REGSConfiguration)?;

        Ok(())
    }

    #[cfg(target_arch = "riscv64")]
    /// Configures an riscv64 specific vcpu.
    ///
    /// # Arguments
    ///
    /// * `vm_fd` - The kvm `VmFd` for this microvm.
    /// * `guest_mem` - The guest memory used by this microvm.
    /// * `kernel_load_addr` - Offset from `guest_mem` at which the kernel is loaded.
    pub fn configure_riscv64(
        &mut self,
        _vm_fd: &VmFd,
        guest_mem: &GuestMemoryMmap,
        kernel_load_addr: GuestAddress,
    ) -> Result<()> {
        arch::riscv64::regs::setup_regs(&self.fd, self.id, kernel_load_addr.raw_value(), guest_mem)
            .map_err(Error::REGSConfiguration)?;
        Ok(())
    }

    /// Moves the vcpu to its own thread and constructs a VcpuHandle.
    /// The handle can be used to control the remote vcpu.
    pub fn start_threaded(mut self) -> Result<VcpuHandle> {
        let event_sender = self.event_sender.take().unwrap();
        let response_receiver = self.response_receiver.take().unwrap();
        let (init_tls_sender, init_tls_receiver) = unbounded();
        let vcpu_thread = thread::Builder::new()
            .name(format!("fc_vcpu {}", self.cpu_index()))
            .spawn(move || {
                self.init_thread_local_data()
                    .expect("Cannot cleanly initialize vcpu TLS.");

                init_tls_sender
                    .send(true)
                    .expect("Cannot notify vcpu TLS initialization.");

                self.run();
            })
            .map_err(Error::VcpuSpawn)?;

        init_tls_receiver
            .recv()
            .expect("Error waiting for TLS initialization.");

        Ok(VcpuHandle::new(
            event_sender,
            response_receiver,
            vcpu_thread,
        ))
    }

    #[allow(unused)]
    #[cfg(target_arch = "x86_64")]
    fn save_state(&self) -> Result<VcpuState> {
        /*
         * Ordering requirements:
         *
         * KVM_GET_MP_STATE calls kvm_apic_accept_events(), which might modify
         * vCPU/LAPIC state. As such, it must be done before most everything
         * else, otherwise we cannot restore everything and expect it to work.
         *
         * KVM_GET_VCPU_EVENTS/KVM_SET_VCPU_EVENTS is unsafe if other vCPUs are
         * still running.
         *
         * KVM_GET_LAPIC may change state of LAPIC before returning it.
         *
         * GET_VCPU_EVENTS should probably be last to save. The code looks as
         * it might as well be affected by internal state modifications of the
         * GET ioctls.
         *
         * SREGS saves/restores a pending interrupt, similar to what
         * VCPU_EVENTS also does.
         *
         * GET_MSRS requires a pre-populated data structure to do something
         * meaningful. For SET_MSRS it will then contain good data.
         */

        // Build the list of MSRs we want to save.
        let num_msrs = self.msr_list.as_fam_struct_ref().nmsrs as usize;
        let mut msrs = Msrs::new(num_msrs).unwrap();
        {
            let indices = self.msr_list.as_slice();
            let msr_entries = msrs.as_mut_slice();
            assert_eq!(indices.len(), msr_entries.len());
            for (pos, index) in indices.iter().enumerate() {
                msr_entries[pos].index = *index;
            }
        }
        let mp_state = self.fd.get_mp_state().map_err(Error::VcpuGetMpState)?;
        let regs = self.fd.get_regs().map_err(Error::VcpuGetRegs)?;
        let sregs = self.fd.get_sregs().map_err(Error::VcpuGetSregs)?;
        let xsave = self.fd.get_xsave().map_err(Error::VcpuGetXsave)?;
        let xcrs = self.fd.get_xcrs().map_err(Error::VcpuGetXcrs)?;
        let debug_regs = self.fd.get_debug_regs().map_err(Error::VcpuGetDebugRegs)?;
        let lapic = self.fd.get_lapic().map_err(Error::VcpuGetLapic)?;
        let nmsrs = self.fd.get_msrs(&mut msrs).map_err(Error::VcpuGetMsrs)?;
        assert_eq!(nmsrs, num_msrs);
        let vcpu_events = self
            .fd
            .get_vcpu_events()
            .map_err(Error::VcpuGetVcpuEvents)?;
        Ok(VcpuState {
            cpuid: self.cpuid.clone(),
            msrs,
            debug_regs,
            lapic,
            mp_state,
            regs,
            sregs,
            vcpu_events,
            xcrs,
            xsave,
        })
    }

    #[allow(unused)]
    #[cfg(target_arch = "x86_64")]
    fn restore_state(&self, state: VcpuState) -> Result<()> {
        /*
         * Ordering requirements:
         *
         * KVM_GET_VCPU_EVENTS/KVM_SET_VCPU_EVENTS is unsafe if other vCPUs are
         * still running.
         *
         * Some SET ioctls (like set_mp_state) depend on kvm_vcpu_is_bsp(), so
         * if we ever change the BSP, we have to do that before restoring anything.
         * The same seems to be true for CPUID stuff.
         *
         * SREGS saves/restores a pending interrupt, similar to what
         * VCPU_EVENTS also does.
         *
         * SET_REGS clears pending exceptions unconditionally, thus, it must be
         * done before SET_VCPU_EVENTS, which restores it.
         *
         * SET_LAPIC must come after SET_SREGS, because the latter restores
         * the apic base msr.
         *
         * SET_LAPIC must come before SET_MSRS, because the TSC deadline MSR
         * only restores successfully, when the LAPIC is correctly configured.
         */
        self.fd
            .set_cpuid2(&state.cpuid)
            .map_err(Error::VcpuSetCpuid)?;
        self.fd
            .set_mp_state(state.mp_state)
            .map_err(Error::VcpuSetMpState)?;
        self.fd.set_regs(&state.regs).map_err(Error::VcpuSetRegs)?;
        self.fd
            .set_sregs(&state.sregs)
            .map_err(Error::VcpuSetSregs)?;
        unsafe {
            self.fd
                .set_xsave(&state.xsave)
                .map_err(Error::VcpuSetXsave)?;
        }
        self.fd.set_xcrs(&state.xcrs).map_err(Error::VcpuSetXcrs)?;
        self.fd
            .set_debug_regs(&state.debug_regs)
            .map_err(Error::VcpuSetDebugRegs)?;
        self.fd
            .set_lapic(&state.lapic)
            .map_err(Error::VcpuSetLapic)?;
        self.fd.set_msrs(&state.msrs).map_err(Error::VcpuSetMsrs)?;
        self.fd
            .set_vcpu_events(&state.vcpu_events)
            .map_err(Error::VcpuSetVcpuEvents)?;
        Ok(())
    }

    #[allow(unused)]
    #[cfg(target_arch = "aarch64")]
    fn save_state(&self) -> Result<VcpuState> {
        let mut reg_list = RegList::new(1).map_err(|_| Error::VcpuUnhandledKvmExit)?;
        let _ = self.fd.get_reg_list(&mut reg_list);
        let count = unsafe { reg_list.as_mut_fam_struct() }.n as usize;
        let mut reg_list = RegList::new(count).map_err(|_| Error::VcpuUnhandledKvmExit)?;
        self.fd.get_reg_list(&mut reg_list).map_err(Error::VcpuFd)?;

        let mut regs = Vec::with_capacity(count);
        for reg_id in reg_list.as_slice() {
            let size = one_reg_size(*reg_id)?;
            let mut value = vec![0; size];
            self.fd
                .get_one_reg(*reg_id, &mut value)
                .map_err(Error::VcpuFd)?;
            regs.push(Aarch64OneReg { id: *reg_id, value });
        }
        regs.sort_by_key(|reg| reg.id);
        let mp_state = self.fd.get_mp_state().ok();
        let vcpu_events = self.fd.get_vcpu_events().ok().map(|events| unsafe {
            std::slice::from_raw_parts(
                &events as *const kvm_vcpu_events as *const u8,
                std::mem::size_of::<kvm_vcpu_events>(),
            )
            .to_vec()
        });
        Ok(VcpuState {
            regs,
            mp_state,
            vcpu_events,
        })
    }

    #[allow(unused)]
    #[cfg(target_arch = "aarch64")]
    fn restore_state(&self, state: VcpuState) -> Result<()> {
        for reg in state.regs {
            self.fd
                .set_one_reg(reg.id, &reg.value)
                .map_err(Error::VcpuFd)?;
        }
        if let Some(vcpu_events) = state.vcpu_events {
            if vcpu_events.len() != std::mem::size_of::<kvm_vcpu_events>() {
                return Err(Error::VcpuUnhandledKvmExit);
            }
            let mut events = std::mem::MaybeUninit::<kvm_vcpu_events>::zeroed();
            unsafe {
                std::ptr::copy_nonoverlapping(
                    vcpu_events.as_ptr(),
                    events.as_mut_ptr() as *mut u8,
                    vcpu_events.len(),
                );
            }
            let vcpu_events = unsafe { events.assume_init() };
            self.fd
                .set_vcpu_events(&vcpu_events)
                .map_err(Error::VcpuSetVcpuEvents)?;
        }
        if let Some(mp_state) = state.mp_state {
            self.fd
                .set_mp_state(mp_state)
                .map_err(Error::VcpuSetMpState)?;
        }
        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    fn restore_hvf_state(&mut self, bytes: &[u8], capture_counter: u64) -> Result<()> {
        let hvf_state = bincode::deserialize::<HvfVcpuStateCompat>(bytes)
            .map_err(|_| Error::VcpuUnhandledKvmExit)?;
        debug!(
            "hvf.restore.input pc=0x{:x} cpsr=0x{:x} sysregs={} gic_icc={} gic_redist={} gic_ich={} vtimer_offset=0x{:x} capture_counter=0x{:x}",
            hvf_state.pc,
            hvf_state.cpsr,
            hvf_state.sysregs.len(),
            hvf_state.gic_icc_regs.len(),
            hvf_state.gic_redist_regs.len(),
            hvf_state.gic_ich_regs.len(),
            hvf_state.vtimer_offset,
            capture_counter
        );
        let mut state = self.save_state()?;
        merge_hvf_state_into_kvm_state(&mut state, &hvf_state, capture_counter)?;
        let timer_state = hvf_timer_restore_state(&state);
        self.restore_state(state)?;
        self.reapply_hvf_core_state_after_mp_state(&hvf_state)?;
        self.log_hvf_restore_readback();
        self.pending_hvf_timer_restore = Some(timer_state);
        self.restore_debug_exits_remaining = 64;
        Ok(())
    }

    #[cfg(not(target_arch = "aarch64"))]
    fn restore_hvf_state(&mut self, _bytes: &[u8], _capture_counter: u64) -> Result<()> {
        Err(Error::VcpuUnhandledKvmExit)
    }

    #[cfg(target_arch = "aarch64")]
    fn get_one_reg_u64(&self, reg_id: u64) -> Option<u64> {
        let mut bytes = [0u8; 8];
        self.fd.get_one_reg(reg_id, &mut bytes).ok()?;
        Some(u64::from_le_bytes(bytes))
    }

    #[cfg(target_arch = "aarch64")]
    fn set_one_reg_u64(&self, reg_id: u64, value: u64) -> Result<()> {
        self.fd
            .set_one_reg(reg_id, &value.to_le_bytes())
            .map(|_| ())
            .map_err(Error::VcpuFd)
    }

    #[cfg(target_arch = "aarch64")]
    fn reapply_hvf_core_state_after_mp_state(&self, hvf: &HvfVcpuStateCompat) -> Result<()> {
        if std::env::var_os("KRUN_REAPPLY_HVF_CORE_AFTER_MP_STATE").is_none() {
            return Ok(());
        }
        self.set_one_reg_u64(core_user_pc_id(), hvf.pc)?;
        self.set_one_reg_u64(core_user_pstate_id(), hvf.cpsr)?;
        for &(reg, value) in &hvf.sysregs {
            match reg {
                reg if reg == hvf_sys_reg(3, 0, 4, 1, 0) => {
                    self.set_one_reg_u64(core_user_sp_id(), value)?
                }
                reg if reg == hvf_sys_reg(3, 4, 4, 1, 0) => {
                    self.set_one_reg_u64(core_sp_el1_id(), value)?
                }
                reg if reg == hvf_sys_reg(3, 0, 4, 0, 0) => {
                    self.set_one_reg_u64(core_spsr_id(0), value)?
                }
                reg if reg == hvf_sys_reg(3, 0, 4, 0, 1) => {
                    self.set_one_reg_u64(core_elr_el1_id(), value)?
                }
                _ => {}
            }
        }
        debug!("hvf.restore.reapply_core_after_mp_state");
        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    fn rearm_hvf_timer_state(&self, timer: HvfTimerRestoreState) -> Result<()> {
        let timer = rearmed_hvf_timer_state(timer);
        if let Some(value) = timer.cnt {
            self.set_one_reg_u64(kvm_timer_counter_id(), value)?;
        }
        if let Some(value) = timer.cval {
            self.set_one_reg_u64(kvm_timer_cval_id(), value)?;
        }
        if let Some(value) = timer.ctl {
            self.set_one_reg_u64(kvm_timer_ctl_id(), value)?;
        }
        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    fn rearm_pending_hvf_timer_state(&mut self) -> Result<()> {
        if let Some(timer) = self.pending_hvf_timer_restore.take() {
            self.rearm_hvf_timer_state(timer)?;
            self.log_hvf_restore_readback();
        }
        Ok(())
    }

    #[cfg(not(target_arch = "aarch64"))]
    fn rearm_pending_hvf_timer_state(&mut self) -> Result<()> {
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    fn get_msr(&self, index: u32) -> Result<u64> {
        let mut msrs = Msrs::from_entries(&[kvm_msr_entry {
            index,
            ..Default::default()
        }])
        .map_err(|_| Error::VcpuUnhandledKvmExit)?;
        let nmsrs = self.fd.get_msrs(&mut msrs).map_err(Error::VcpuGetMsrs)?;
        if nmsrs != 1 {
            return Err(Error::VcpuUnhandledKvmExit);
        }
        Ok(msrs.as_slice()[0].data)
    }

    #[cfg(target_arch = "x86_64")]
    fn set_msr(&self, index: u32, data: u64) -> Result<()> {
        let msrs = Msrs::from_entries(&[kvm_msr_entry {
            index,
            data,
            ..Default::default()
        }])
        .map_err(|_| Error::VcpuUnhandledKvmExit)?;
        let nmsrs = self.fd.set_msrs(&msrs).map_err(Error::VcpuSetMsrs)?;
        if nmsrs != 1 {
            return Err(Error::VcpuUnhandledKvmExit);
        }
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    fn advance_deterministic_time_if_blocked(&self) -> Result<bool> {
        let mp_state = self.fd.get_mp_state().map_err(Error::VcpuGetMpState)?;
        if mp_state.mp_state != KVM_MP_STATE_HALTED {
            return Ok(false);
        }
        if !deterministic_host_idle() {
            return Ok(false);
        }

        let deadline = self.get_msr(MSR_IA32_TSC_DEADLINE)?;
        if deadline == 0 {
            return Ok(false);
        }

        let tsc = self.get_msr(MSR_IA32_TSC)?;
        if deadline <= tsc {
            return Ok(false);
        }

        self.set_msr(MSR_IA32_TSC, deadline)?;
        self.set_msr(MSR_IA32_TSC_DEADLINE, deadline)?;
        record_deterministic_timer_jump(deadline, DETERMINISTIC_X86_TSC_HZ)?;
        debug!(
            "deterministic_time.jump_halted_vcpu vcpu={} previous_tsc={} deadline_tsc={}",
            self.id, tsc, deadline
        );
        Ok(true)
    }

    #[cfg(target_arch = "aarch64")]
    fn advance_deterministic_time_if_blocked(&self) -> Result<bool> {
        deterministic_time_debug_event(format!("deterministic_time.arm.wfi_exit vcpu={}", self.id));
        if !deterministic_host_idle() {
            deterministic_time_debug_event(format!(
                "deterministic_time.arm.skip_host_activity vcpu={}",
                self.id
            ));
            return Ok(false);
        }

        let Some(timer) = self.next_deterministic_timer_deadline()? else {
            return Ok(false);
        };

        let jump_to = timer.cval;
        let delta = jump_to - timer.cnt;
        self.set_one_reg_u64(timer.cnt_reg, jump_to)?;
        deterministic_time_debug_event(format!(
            "deterministic_time.arm.jump_counter vcpu={} timer={} delta={}",
            self.id, timer.name, delta
        ));
        self.set_one_reg_u64(timer.cval_reg, timer.cval)?;
        self.set_one_reg_u64(timer.ctl_reg, timer.ctl & !TMR_CTL_ISTATUS)?;
        let counter_frequency_hz = self
            .get_one_reg_u64(arm64_sys_reg_id(3, 3, 14, 0, 0))
            .unwrap_or(DETERMINISTIC_ARM_COUNTER_HZ);
        record_deterministic_timer_jump(jump_to, counter_frequency_hz)?;
        debug!(
            "deterministic_time.jump_halted_vcpu vcpu={} timer={} previous_cnt={} deadline_cnt={} jump_cnt={}",
            self.id, timer.name, timer.cnt, timer.cval, jump_to
        );
        Ok(true)
    }

    #[cfg(target_arch = "aarch64")]
    fn next_deterministic_timer_deadline(&self) -> Result<Option<DeterministicTimerDeadline>> {
        let mut selected: Option<DeterministicTimerDeadline> = None;
        for timer in deterministic_timer_registers() {
            let Some(ctl) = self.get_one_reg_u64(timer.ctl_reg) else {
                deterministic_time_debug_event(format!(
                    "deterministic_time.arm.skip_no_ctl vcpu={} timer={}",
                    self.id, timer.name
                ));
                continue;
            };
            if (ctl & TMR_CTL_ENABLE) == 0 || (ctl & TMR_CTL_IMASK) != 0 {
                deterministic_time_debug_event(format!(
                    "deterministic_time.arm.skip_ctl vcpu={} timer={} ctl=0x{:x}",
                    self.id, timer.name, ctl
                ));
                continue;
            }
            let Some(cval) = self.get_one_reg_u64(timer.cval_reg) else {
                deterministic_time_debug_event(format!(
                    "deterministic_time.arm.skip_no_cval vcpu={} timer={}",
                    self.id, timer.name
                ));
                continue;
            };
            let Some(cnt) = self.get_one_reg_u64(timer.cnt_reg) else {
                deterministic_time_debug_event(format!(
                    "deterministic_time.arm.skip_no_cnt vcpu={} timer={}",
                    self.id, timer.name
                ));
                continue;
            };
            if cval <= cnt {
                deterministic_time_debug_event(format!(
                    "deterministic_time.arm.skip_past_deadline vcpu={} timer={} cval={} cnt={}",
                    self.id, timer.name, cval, cnt
                ));
                continue;
            }
            let deadline = DeterministicTimerDeadline {
                name: timer.name,
                ctl_reg: timer.ctl_reg,
                cval_reg: timer.cval_reg,
                cnt_reg: timer.cnt_reg,
                ctl,
                cval,
                cnt,
            };
            let should_select = match selected.as_ref() {
                Some(current) => cval - cnt < current.cval - current.cnt,
                None => true,
            };
            if should_select {
                selected = Some(deadline);
            }
        }
        Ok(selected)
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    fn advance_deterministic_time_if_blocked(&self) -> Result<bool> {
        Ok(false)
    }

    #[cfg(target_arch = "aarch64")]
    fn log_hvf_restore_readback(&self) {
        let regs = vec![
            ("x0", core_user_reg_id(0)),
            ("x1", core_user_reg_id(1)),
            ("x2", core_user_reg_id(2)),
            ("x3", core_user_reg_id(3)),
            ("x4", core_user_reg_id(4)),
            ("x5", core_user_reg_id(5)),
            ("x6", core_user_reg_id(6)),
            ("x7", core_user_reg_id(7)),
            ("x8", core_user_reg_id(8)),
            ("x9", core_user_reg_id(9)),
            ("x29", core_user_reg_id(29)),
            ("x30", core_user_reg_id(30)),
            ("pc", core_user_pc_id()),
            ("pstate", core_user_pstate_id()),
            ("sp_el0", core_user_sp_id()),
            ("sp_el1", core_sp_el1_id()),
            ("sys_sp_el0", arm64_sys_reg_id(3, 0, 4, 1, 0)),
            ("currentel", arm64_sys_reg_id(3, 0, 4, 2, 2)),
            ("spsel", arm64_sys_reg_id(3, 0, 4, 2, 0)),
            ("elr_el1", core_elr_el1_id()),
            ("spsr_el1", core_spsr_id(0)),
            ("esr_el1", arm64_sys_reg_id(3, 0, 5, 2, 0)),
            ("far_el1", arm64_sys_reg_id(3, 0, 6, 0, 0)),
            ("sctlr_el1", arm64_sys_reg_id(3, 0, 1, 0, 0)),
            ("ttbr0_el1", arm64_sys_reg_id(3, 0, 2, 0, 0)),
            ("ttbr1_el1", arm64_sys_reg_id(3, 0, 2, 0, 1)),
            ("tcr_el1", arm64_sys_reg_id(3, 0, 2, 0, 2)),
            ("mair_el1", arm64_sys_reg_id(3, 0, 10, 2, 0)),
            ("amair_el1", arm64_sys_reg_id(3, 0, 10, 3, 0)),
            ("contextidr_el1", arm64_sys_reg_id(3, 0, 13, 0, 1)),
            ("tpidr_el0", arm64_sys_reg_id(3, 3, 13, 0, 2)),
            ("tpidrro_el0", arm64_sys_reg_id(3, 3, 13, 0, 3)),
            ("tpidr_el1", arm64_sys_reg_id(3, 0, 13, 0, 4)),
            ("vbar_el1", arm64_sys_reg_id(3, 0, 12, 0, 0)),
            ("cntv_ctl", kvm_timer_ctl_id()),
            ("cntv_cval", kvm_timer_cval_id()),
            ("cntv_cnt", kvm_timer_counter_id()),
        ];
        let esr_el1 = self.get_one_reg_u64(arm64_sys_reg_id(3, 0, 5, 2, 0));
        let summary = regs
            .iter()
            .map(|(name, id)| {
                self.get_one_reg_u64(*id)
                    .map(|value| format!("{name}=0x{value:x}"))
                    .unwrap_or_else(|| format!("{name}=<missing>"))
            })
            .collect::<Vec<_>>()
            .join(" ");
        let esr_summary = esr_el1
            .map(decode_esr_el1_summary)
            .unwrap_or_else(|| "esr=<missing>".to_string());
        debug!("hvf.restore.readback {summary} {esr_summary}");
    }

    #[cfg(not(target_arch = "aarch64"))]
    fn log_hvf_restore_readback(&self) {}

    #[cfg(target_arch = "aarch64")]
    fn log_restore_debug_exit(&mut self, exit: std::fmt::Arguments<'_>) {
        if self.restore_debug_exits_remaining == 0 {
            return;
        }
        debug!(
            "hvf.restore.kvm_run.exit vcpu={} remaining={} {exit}",
            self.id, self.restore_debug_exits_remaining
        );
        self.restore_debug_exits_remaining -= 1;
    }

    /// Re-arm deadlines that are measured against the host-advancing physical
    /// counter. The virtual timer needs no rebase: KVM_REG_ARM_TIMER_CNT is
    /// restored from the saved register list, which freezes guest virtual
    /// time across the suspension and keeps CNTV_CVAL valid as captured.
    ///
    /// Assumes guest CNTPCT tracks the host counter (no
    /// KVM_ARM_VM_COUNTER_OFFSET in use); revisit the physical-timer delta if
    /// that ever changes.
    ///
    /// NOTE: KVM_REG_ARM_TIMER_CVAL and KVM_REG_ARM_TIMER_CNT have
    /// historically swapped sysreg encodings — (3,3,14,0,2) is the virtual
    /// CVAL and (3,3,14,3,2) is the virtual counter. Do not address the
    /// virtual timer by its architectural encodings.
    #[cfg(target_arch = "aarch64")]
    fn rebase_timer(&self, delta_ticks: u64) -> Result<()> {
        for (cval_reg, ctl_reg) in [
            // CNTP_CVAL_EL0 / CNTP_CTL_EL0
            (
                arm64_sys_reg_id(3, 3, 14, 2, 2),
                arm64_sys_reg_id(3, 3, 14, 2, 1),
            ),
            // CNTHP_CVAL_EL2 / CNTHP_CTL_EL2 (present only with nested virt)
            (
                arm64_sys_reg_id(3, 4, 14, 2, 2),
                arm64_sys_reg_id(3, 4, 14, 2, 1),
            ),
        ] {
            if let Some(cval) = self.get_one_reg_u64(cval_reg) {
                self.set_one_reg_u64(cval_reg, cval.wrapping_add(delta_ticks))?;
            }
            if let Some(ctl) = self.get_one_reg_u64(ctl_reg) {
                let ctl = if (ctl & TMR_CTL_ENABLE) == 0 {
                    ctl & !TMR_CTL_ISTATUS
                } else {
                    ctl
                };
                self.set_one_reg_u64(ctl_reg, ctl)?;
            }
        }
        Ok(())
    }

    #[cfg(not(target_arch = "aarch64"))]
    fn rebase_timer(&self, _delta_ticks: u64) -> Result<()> {
        Ok(())
    }

    /// Runs the vCPU in KVM context and handles the kvm exit reason.
    ///
    /// Returns error or enum specifying whether emulation was handled or interrupted.
    fn run_emulation(&mut self) -> Result<VcpuEmulation> {
        // This is a workaround for a kernel bug in the Linux
        // kernel (6.12 and 6.13).
        // https://github.com/containers/libkrun/issues/314#issuecomment-2818154193
        #[cfg(target_arch = "x86_64")]
        {
            if self.kernel_enomem_workaround {
                thread::sleep(std::time::Duration::from_millis(5));
            }
        }

        #[cfg(target_arch = "aarch64")]
        if self.restore_debug_exits_remaining > 0 {
            debug!(
                "hvf.restore.kvm_run.enter vcpu={} remaining={}",
                self.id, self.restore_debug_exits_remaining
            );
        }

        match self.fd.run() {
            Ok(run) => match run {
                #[cfg(feature = "tee")]
                VcpuExit::Hypercall(hypercall) => {
                    if hypercall.nr != 12
                    /* KVM_HC_MAP_GPA_RANGE */
                    {
                        return Err(Error::VcpuUnsupportedHypercall);
                    }

                    let gpa = hypercall.args[0];
                    let size = hypercall.args[1] * 0x1000; /* TARGET_PAGE_SIZE */
                    let attributes = hypercall.args[2];

                    let private = !matches!(attributes, 0);

                    let mem_properties = MemoryProperties { gpa, size, private };

                    let (response_sender, response_receiver) = unbounded();
                    self.pm_sender
                        .send(WorkerMessage::ConvertMemory(
                            response_sender.clone(),
                            mem_properties,
                        ))
                        .unwrap();
                    if !response_receiver.recv().unwrap() {
                        error!(
                            "Unable to convert memory with properties: gpa: 0x{gpa:x} size: 0x{size:x} to_private: {private}"
                        );
                        return Err(Error::VcpuUnhandledKvmExit);
                    }
                    Ok(VcpuEmulation::Handled)
                }
                #[cfg(target_arch = "x86_64")]
                VcpuExit::IoIn(addr, data) => {
                    self.io_bus.read(0, u64::from(addr), data);
                    Ok(VcpuEmulation::Handled)
                }
                #[cfg(target_arch = "x86_64")]
                VcpuExit::IoOut(addr, data) => {
                    self.io_bus.write(0, u64::from(addr), data);
                    Ok(VcpuEmulation::Handled)
                }
                #[cfg(feature = "tee")]
                VcpuExit::MemoryFault { gpa, size, flags } => {
                    if flags & !kvm_bindings::KVM_MEMORY_EXIT_FLAG_PRIVATE as u64 != 0 {
                        println!("KVM_EXIT_MEMORY_FAULT: Unknown flag {flags}");
                        Err(Error::VcpuUnhandledKvmExit)
                    } else {
                        let private = (flags & (KVM_MEMORY_EXIT_FLAG_PRIVATE as u64)) != 0;
                        let mem_properties = MemoryProperties { gpa, size, private };
                        let (response_sender, response_receiver) = unbounded();
                        self.pm_sender
                            .send(WorkerMessage::ConvertMemory(
                                response_sender.clone(),
                                mem_properties,
                            ))
                            .unwrap();
                        if !response_receiver.recv().unwrap() {
                            error!(
                                "Unable to convert memory with properties: gpa: 0x{gpa:x} size: 0x{size:x} to_private: {private}"
                            );
                            return Err(Error::VcpuUnhandledKvmExit);
                        }
                        Ok(VcpuEmulation::Handled)
                    }
                }
                VcpuExit::MmioRead(addr, data) => {
                    let len = data.len();
                    if let Some(ref mmio_bus) = self.mmio_bus {
                        mmio_bus.read(0, addr, data);
                    }
                    #[cfg(target_arch = "aarch64")]
                    self.log_restore_debug_exit(format_args!("MmioRead addr=0x{addr:x} len={len}"));
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::MmioWrite(addr, data) => {
                    let len = data.len();
                    if let Some(ref mmio_bus) = self.mmio_bus {
                        mmio_bus.write(0, addr, data);
                    }
                    #[cfg(target_arch = "aarch64")]
                    self.log_restore_debug_exit(format_args!(
                        "MmioWrite addr=0x{addr:x} len={len}"
                    ));
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::Hlt => {
                    #[cfg(target_arch = "aarch64")]
                    self.log_restore_debug_exit(format_args!("Hlt"));
                    if deterministic_time_enabled()
                        && self.advance_deterministic_time_if_blocked()?
                    {
                        return Ok(VcpuEmulation::Handled);
                    }
                    #[cfg(target_arch = "aarch64")]
                    if deterministic_time_enabled() {
                        return Ok(VcpuEmulation::Handled);
                    }
                    info!("Received KVM_EXIT_HLT signal");
                    Ok(VcpuEmulation::Stopped)
                }
                VcpuExit::Shutdown => {
                    #[cfg(target_arch = "aarch64")]
                    self.log_restore_debug_exit(format_args!("Shutdown"));
                    info!("Received KVM_EXIT_SHUTDOWN signal");
                    Ok(VcpuEmulation::Stopped)
                }
                // Documentation specifies that below kvm exits are considered
                // errors.
                VcpuExit::FailEntry(reason, vcpu) => {
                    #[cfg(target_arch = "aarch64")]
                    self.log_restore_debug_exit(format_args!(
                        "FailEntry reason={reason} vcpu={vcpu}"
                    ));
                    error!("Received KVM_EXIT_FAIL_ENTRY signal: reason={reason}, vcpu={vcpu}");
                    Err(Error::VcpuUnhandledKvmExit)
                }
                VcpuExit::InternalError => {
                    #[cfg(target_arch = "aarch64")]
                    self.log_restore_debug_exit(format_args!("InternalError"));
                    error!("Received KVM_EXIT_INTERNAL_ERROR signal");
                    Err(Error::VcpuUnhandledKvmExit)
                }
                VcpuExit::SystemEvent(event, _reason) => {
                    #[cfg(target_arch = "aarch64")]
                    self.log_restore_debug_exit(format_args!("SystemEvent event={event}"));
                    match event {
                        KVM_SYSTEM_EVENT_SHUTDOWN => {
                            info!("Received KVM_SYSTEM_EVENT_SHUTDOWN")
                        }
                        KVM_SYSTEM_EVENT_RESET => info!("Received KVM_SYSTEM_EVENT_RESET"),
                        _ => error!("Received an unexpected System Event: {event}"),
                    }
                    Ok(VcpuEmulation::Stopped)
                }
                r => {
                    // TODO: Are we sure we want to finish running a vcpu upon
                    // receiving a vm exit that is not necessarily an error?
                    error!("Unexpected exit reason on vcpu run: {r:?}");
                    Err(Error::VcpuUnhandledKvmExit)
                }
            },
            // The unwrap on raw_os_error can only fail if we have a logic
            // error in our code in which case it is better to panic.
            Err(ref e) => match e.errno() {
                libc::EAGAIN => Ok(VcpuEmulation::Handled),
                libc::EINTR => {
                    #[cfg(target_arch = "aarch64")]
                    {
                        self.log_restore_debug_exit(format_args!("EINTR"));
                        if self.restore_debug_exits_remaining > 0 {
                            self.log_hvf_restore_readback();
                        }
                    }
                    self.fd.set_kvm_immediate_exit(0);
                    // Notify that this KVM_RUN was interrupted.
                    Ok(VcpuEmulation::Interrupted)
                }
                _ => {
                    error!("Failure during vcpu run: {e}");
                    Err(Error::VcpuUnhandledKvmExit)
                }
            },
        }
    }

    /// Main loop of the vCPU thread.
    ///
    /// Runs the vCPU in KVM context in a loop. Handles KVM_EXITs then goes back in.
    /// Note that the state of the VCPU and associated VM must be setup first for this to do
    /// anything useful.
    pub fn run(&mut self) {
        // Start running the machine state in the `Paused` state.
        StateMachine::run(self, Self::paused);
    }

    // This is the main loop of the `Running` state.
    fn running(&mut self) -> StateMachine<Self> {
        // This loop is here just for optimizing the emulation path.
        // No point in ticking the state machine if there are no external events.
        loop {
            match self.run_emulation() {
                // Emulation ran successfully, continue.
                Ok(VcpuEmulation::Handled) => (),
                // Emulation was interrupted, check external events.
                Ok(VcpuEmulation::Interrupted) => break,
                // If the guest was rebooted or halted:
                // - vCPU0 will always exit out of `KVM_RUN` with KVM_EXIT_SHUTDOWN or
                //   KVM_EXIT_HLT.
                // - the other vCPUs won't ever exit out of `KVM_RUN`, but they won't consume CPU.
                // Moreover if we allow the vCPU0 thread to finish execution, this might generate a
                // seccomp failure because musl calls `sigprocmask` as part of `pthread_exit`.
                // So we pause vCPU0 and send a signal to the emulation thread to stop the VMM.
                Ok(VcpuEmulation::Stopped) => return self.exit(FC_EXIT_CODE_OK),
                // Emulation errors lead to vCPU exit.
                Err(_) => return self.exit(FC_EXIT_CODE_GENERIC_ERROR),
            }
        }

        // By default don't change state.
        let mut state = StateMachine::next(Self::running);

        // Break this emulation loop on any transition request/external event.
        match self.event_receiver.try_recv() {
            // Running ---- Pause ----> Paused
            Ok(VcpuEvent::Pause) => {
                match self.save_state().and_then(|state| {
                    bincode::serialize(&state).map_err(|_| Error::VcpuUnhandledKvmExit)
                }) {
                    Ok(bytes) => self
                        .response_sender
                        .send(VcpuResponse::Paused(bytes))
                        .expect("failed to send pause status"),
                    Err(e) => self
                        .response_sender
                        .send(VcpuResponse::Error(format!("save vcpu state: {e}")))
                        .expect("failed to send pause error"),
                }

                // TODO: we should call `KVM_KVMCLOCK_CTRL` here to make sure
                // TODO continued: the guest soft lockup watchdog does not panic on Resume.

                // Move to 'paused' state.
                state = StateMachine::next(Self::paused);
            }
            Ok(VcpuEvent::Resume) => {
                self.response_sender
                    .send(VcpuResponse::Resumed)
                    .expect("failed to send resume status");
            }
            Ok(VcpuEvent::RestoreState(_)) => {
                self.response_sender
                    .send(VcpuResponse::Error("not paused".into()))
                    .expect("failed to send restore error");
            }
            Ok(VcpuEvent::RestoreHvfState { .. }) => {
                self.response_sender
                    .send(VcpuResponse::Error("not paused".into()))
                    .expect("failed to send restore error");
            }
            Ok(VcpuEvent::RebaseTimer(_)) => {
                self.response_sender
                    .send(VcpuResponse::Error("not paused".into()))
                    .expect("failed to send timer rebase error");
            }
            // Unhandled exit of the other end.
            Err(TryRecvError::Disconnected) => {
                // Move to 'exited' state.
                state = self.exit(FC_EXIT_CODE_GENERIC_ERROR);
            }
            // All other events or lack thereof have no effect on current 'running' state.
            Err(TryRecvError::Empty) => (),
        }

        state
    }

    // This is the main loop of the `Paused` state.
    fn paused(&mut self) -> StateMachine<Self> {
        match self.event_receiver.recv() {
            // Paused ---- Resume ----> Running
            Ok(VcpuEvent::Resume) => {
                // Nothing special to do.
                self.response_sender
                    .send(VcpuResponse::Resumed)
                    .expect("failed to send resume status");
                // Move to 'running' state.
                StateMachine::next(Self::running)
            }
            Ok(VcpuEvent::RestoreState(bytes)) => {
                let result = bincode::deserialize::<VcpuState>(&bytes)
                    .map_err(|_| Error::VcpuUnhandledKvmExit)
                    .and_then(|state| self.restore_state(state));
                match result {
                    Ok(()) => self
                        .response_sender
                        .send(VcpuResponse::Restored)
                        .expect("failed to send restore status"),
                    Err(e) => self
                        .response_sender
                        .send(VcpuResponse::Error(format!("restore vcpu state: {e}")))
                        .expect("failed to send restore error"),
                }
                StateMachine::next(Self::paused)
            }
            Ok(VcpuEvent::RestoreHvfState {
                state,
                capture_counter,
            }) => {
                let result = self.restore_hvf_state(&state, capture_counter);
                match result {
                    Ok(()) => self
                        .response_sender
                        .send(VcpuResponse::Restored)
                        .expect("failed to send restore status"),
                    Err(e) => self
                        .response_sender
                        .send(VcpuResponse::Error(format!("restore hvf vcpu state: {e}")))
                        .expect("failed to send restore error"),
                }
                StateMachine::next(Self::paused)
            }
            Ok(VcpuEvent::RebaseTimer(delta_ticks)) => {
                match self
                    .rebase_timer(delta_ticks)
                    .and_then(|_| self.rearm_pending_hvf_timer_state())
                {
                    Ok(()) => self
                        .response_sender
                        .send(VcpuResponse::TimerRebased)
                        .expect("failed to send timer rebase status"),
                    Err(e) => self
                        .response_sender
                        .send(VcpuResponse::Error(format!("rebase timer: {e}")))
                        .expect("failed to send timer rebase error"),
                }
                StateMachine::next(Self::paused)
            }
            // All other events have no effect on current 'paused' state.
            Ok(_) => StateMachine::next(Self::paused),
            // Unhandled exit of the other end.
            Err(_) => {
                // Move to 'exited' state.
                self.exit(FC_EXIT_CODE_GENERIC_ERROR)
            }
        }
    }

    #[cfg(not(test))]
    // Transition to the exited state.
    fn exit(&mut self, exit_code: u8) -> StateMachine<Self> {
        self.response_sender
            .send(VcpuResponse::Exited(exit_code))
            .expect("failed to send Exited status");

        if let Err(e) = self.exit_evt.write(1) {
            error!("Failed signaling vcpu exit event: {e}");
        }

        // State machine reached its end.
        StateMachine::next(Self::exited)
    }

    #[cfg(not(test))]
    // This is the main loop of the `Exited` state.
    fn exited(&mut self) -> StateMachine<Self> {
        // Wait indefinitely.
        // The VMM thread will kill the entire process.
        let barrier = Barrier::new(2);
        barrier.wait();

        StateMachine::finish()
    }

    #[cfg(feature = "tdx")]
    pub fn tdx_secure_virt_prepare(&self, launcher: &mut tdx::launch::Launcher) {
        use std::os::fd::AsRawFd;
        launcher.add_vcpu_fd(self.fd.as_raw_fd());
    }

    #[cfg(test)]
    // In tests the main/vmm thread exits without 'exit()'ing the whole process.
    // All channels get closed on the other side while this Vcpu thread is still running.
    // This Vcpu thread should just do a clean finish without reporting back to the main thread.
    fn exit(&mut self, _: u8) -> StateMachine<Self> {
        // State machine reached its end.
        StateMachine::finish()
    }
}

impl Drop for Vcpu {
    fn drop(&mut self) {
        let _ = self.reset_thread_local_data();
    }
}

#[cfg(target_arch = "x86_64")]
/// Structure holding VCPU kvm state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VcpuState {
    cpuid: CpuId,
    msrs: Msrs,
    debug_regs: kvm_debugregs,
    lapic: kvm_lapic_state,
    mp_state: kvm_mp_state,
    regs: kvm_regs,
    sregs: kvm_sregs,
    vcpu_events: kvm_vcpu_events,
    xcrs: kvm_xcrs,
    xsave: kvm_xsave,
}

#[cfg(target_arch = "aarch64")]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Aarch64OneReg {
    id: u64,
    value: Vec<u8>,
}

#[cfg(target_arch = "aarch64")]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VcpuState {
    regs: Vec<Aarch64OneReg>,
    #[serde(default)]
    mp_state: Option<kvm_mp_state>,
    #[serde(default)]
    vcpu_events: Option<Vec<u8>>,
}

#[cfg(target_arch = "aarch64")]
#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct HvfVcpuStateCompat {
    gp: [u64; 31],
    pc: u64,
    cpsr: u64,
    fpcr: u64,
    fpsr: u64,
    fp: [u128; 32],
    sysregs: Vec<(u16, u64)>,
    #[serde(default)]
    gic_icc_regs: Vec<(u16, u64)>,
    #[serde(default)]
    gic_redist_regs: Vec<(u32, u64)>,
    #[serde(default)]
    gic_ich_regs: Vec<(u16, u64)>,
    vtimer_masked: bool,
    #[serde(default)]
    vtimer_offset: u64,
}

#[cfg(target_arch = "aarch64")]
#[derive(Clone, Copy, Debug)]
struct HvfTimerRestoreState {
    ctl: Option<u64>,
    cval: Option<u64>,
    cnt: Option<u64>,
}

#[cfg(target_arch = "aarch64")]
const HVF_EXPIRED_TIMER_REARM_TICKS: u64 = 10_000_000;

#[cfg(target_arch = "aarch64")]
pub(crate) fn decode_hvf_vcpu_gic_state(
    bytes: &[u8],
) -> std::result::Result<(Vec<(u16, u64)>, Vec<(u32, u64)>, Vec<(u16, u64)>), bincode::Error> {
    bincode::deserialize::<HvfVcpuStateCompat>(bytes).map(|state| {
        (
            state.gic_icc_regs,
            state.gic_redist_regs,
            state.gic_ich_regs,
        )
    })
}

#[cfg(target_arch = "aarch64")]
fn merge_hvf_state_into_kvm_state(
    state: &mut VcpuState,
    hvf: &HvfVcpuStateCompat,
    capture_counter: u64,
) -> Result<()> {
    for (index, value) in hvf.gp.iter().enumerate() {
        replace_required_reg_u64(state, core_user_reg_id(index), *value)?;
    }
    replace_required_reg_u64(state, core_user_pc_id(), hvf.pc)?;
    replace_required_reg_u64(state, core_user_pstate_id(), hvf.cpsr)?;

    replace_optional_reg_u32(state, core_fp_fpsr_id(), hvf.fpsr as u32)?;
    replace_optional_reg_u32(state, core_fp_fpcr_id(), hvf.fpcr as u32)?;
    for (index, value) in hvf.fp.iter().enumerate() {
        replace_optional_reg_u128(state, core_fp_vreg_id(index), *value)?;
    }

    for &(reg, value) in &hvf.sysregs {
        apply_hvf_sysreg(state, reg, value)?;
    }
    replace_optional_reg_u64(
        state,
        kvm_timer_counter_id(),
        capture_counter.wrapping_sub(hvf.vtimer_offset),
    )?;

    state.mp_state = Some(kvm_mp_state {
        mp_state: KVM_MP_STATE_RUNNABLE,
    });

    Ok(())
}

#[cfg(target_arch = "aarch64")]
fn reg_u64(state: &VcpuState, id: u64) -> Option<u64> {
    let reg = state.regs.iter().find(|reg| reg.id == id)?;
    let bytes = reg.value.get(..8)?;
    Some(u64::from_le_bytes(bytes.try_into().ok()?))
}

#[cfg(target_arch = "aarch64")]
fn apply_hvf_sysreg(state: &mut VcpuState, reg: u16, value: u64) -> Result<()> {
    let value = sanitize_hvf_control_sysreg_for_kvm(reg, value);
    match reg {
        reg if reg == hvf_sys_reg(3, 0, 4, 0, 0) => {
            replace_optional_reg_u64(state, core_spsr_id(0), value)
        }
        reg if reg == hvf_sys_reg(3, 0, 4, 0, 1) => {
            replace_optional_reg_u64(state, core_elr_el1_id(), value)
        }
        reg if reg == hvf_sys_reg(3, 0, 4, 1, 0) => {
            replace_optional_reg_u64(state, core_user_sp_id(), value)
        }
        reg if reg == hvf_sys_reg(3, 4, 4, 1, 0) => {
            if std::env::var_os("KRUN_SKIP_HVF_SP_EL1_RESTORE").is_some() {
                debug!("hvf.restore.skip_sysreg reg=0x{reg:x} name=sp_el1 value=0x{value:x}");
                return Ok(());
            }
            replace_optional_reg_u64(state, core_sp_el1_id(), value)
        }
        reg if reg == hvf_sys_reg(3, 3, 14, 3, 2) => {
            replace_optional_reg_u64(state, kvm_timer_cval_id(), value)
        }
        reg if hvf_sysreg_is_host_derived(reg) => Ok(()),
        reg => replace_optional_reg_u64(state, hvf_sysreg_to_kvm_reg_id(reg), value),
    }
}

#[cfg(target_arch = "aarch64")]
fn sanitize_hvf_control_sysreg_for_kvm(reg: u16, value: u64) -> u64 {
    if std::env::var_os("KRUN_SANITIZE_HVF_CONTROL_SYSREGS").is_none() {
        return value;
    }

    let sanitized = sanitize_hvf_control_sysreg_value(reg, value);
    if sanitized != value {
        debug!(
            "hvf.restore.sanitize_sysreg reg=0x{reg:x} value=0x{value:x} sanitized=0x{sanitized:x}"
        );
    }
    sanitized
}

#[cfg(target_arch = "aarch64")]
fn sanitize_hvf_control_sysreg_value(reg: u16, value: u64) -> u64 {
    match reg {
        reg if reg == hvf_sys_reg(3, 0, 1, 0, 0) => value & u64::from(u32::MAX),
        reg if reg == hvf_sys_reg(3, 0, 2, 0, 2) => value & 0x0000_ffff_ffff_ffff,
        _ => value,
    }
}

#[cfg(target_arch = "aarch64")]
fn decode_esr_el1_summary(esr: u64) -> String {
    let ec = (esr >> 26) & 0x3f;
    let il = (esr >> 25) & 0x1;
    let iss = esr & 0x01ff_ffff;
    let dfsc = iss & 0x3f;
    format!("esr_ec=0x{ec:x} esr_il={il} esr_iss=0x{iss:x} esr_dfsc=0x{dfsc:x}")
}

#[cfg(target_arch = "aarch64")]
fn kvm_timer_cval_id() -> u64 {
    arm64_sys_reg_id(3, 3, 14, 0, 2)
}

#[cfg(target_arch = "aarch64")]
fn kvm_timer_ctl_id() -> u64 {
    arm64_sys_reg_id(3, 3, 14, 3, 1)
}

#[cfg(target_arch = "aarch64")]
fn kvm_timer_counter_id() -> u64 {
    arm64_sys_reg_id(3, 3, 14, 3, 2)
}

#[cfg(target_arch = "aarch64")]
fn kvm_physical_timer_cval_id() -> u64 {
    arm64_sys_reg_id(3, 3, 14, 2, 2)
}

#[cfg(target_arch = "aarch64")]
fn kvm_physical_timer_ctl_id() -> u64 {
    arm64_sys_reg_id(3, 3, 14, 2, 1)
}

#[cfg(target_arch = "aarch64")]
fn kvm_physical_timer_counter_id() -> u64 {
    arm64_sys_reg_id(3, 3, 14, 0, 1)
}

#[cfg(target_arch = "aarch64")]
fn kvm_hyp_physical_timer_cval_id() -> u64 {
    arm64_sys_reg_id(3, 4, 14, 2, 2)
}

#[cfg(target_arch = "aarch64")]
fn kvm_hyp_physical_timer_ctl_id() -> u64 {
    arm64_sys_reg_id(3, 4, 14, 2, 1)
}

#[cfg(target_arch = "aarch64")]
#[derive(Clone, Copy)]
struct DeterministicTimerRegisters {
    name: &'static str,
    ctl_reg: u64,
    cval_reg: u64,
    cnt_reg: u64,
}

#[cfg(target_arch = "aarch64")]
struct DeterministicTimerDeadline {
    name: &'static str,
    ctl_reg: u64,
    cval_reg: u64,
    cnt_reg: u64,
    ctl: u64,
    cval: u64,
    cnt: u64,
}

#[cfg(target_arch = "aarch64")]
fn deterministic_timer_registers() -> [DeterministicTimerRegisters; 3] {
    [
        DeterministicTimerRegisters {
            name: "virtual",
            ctl_reg: kvm_timer_ctl_id(),
            cval_reg: kvm_timer_cval_id(),
            cnt_reg: kvm_timer_counter_id(),
        },
        DeterministicTimerRegisters {
            name: "physical",
            ctl_reg: kvm_physical_timer_ctl_id(),
            cval_reg: kvm_physical_timer_cval_id(),
            cnt_reg: kvm_physical_timer_counter_id(),
        },
        DeterministicTimerRegisters {
            name: "hyp_physical",
            ctl_reg: kvm_hyp_physical_timer_ctl_id(),
            cval_reg: kvm_hyp_physical_timer_cval_id(),
            cnt_reg: kvm_physical_timer_counter_id(),
        },
    ]
}

#[cfg(target_arch = "aarch64")]
fn hvf_timer_restore_state(state: &VcpuState) -> HvfTimerRestoreState {
    HvfTimerRestoreState {
        ctl: reg_u64(state, kvm_timer_ctl_id()),
        cval: reg_u64(state, kvm_timer_cval_id()),
        cnt: reg_u64(state, kvm_timer_counter_id()),
    }
}

#[cfg(target_arch = "aarch64")]
fn rearmed_hvf_timer_state(timer: HvfTimerRestoreState) -> HvfTimerRestoreState {
    let needs_slack = matches!(
        (timer.cval, timer.cnt, timer.ctl),
        (Some(cval), Some(cnt), Some(ctl))
            if (ctl & TMR_CTL_ENABLE) != 0
                && cval.wrapping_sub(cnt) <= HVF_EXPIRED_TIMER_REARM_TICKS
    );
    HvfTimerRestoreState {
        cval: match (timer.cval, timer.cnt) {
            (Some(_), Some(cnt)) if needs_slack => {
                Some(cnt.wrapping_add(HVF_EXPIRED_TIMER_REARM_TICKS))
            }
            _ => timer.cval,
        },
        ctl: if needs_slack {
            timer.ctl.map(|ctl| ctl & !TMR_CTL_ISTATUS)
        } else {
            timer.ctl
        },
        cnt: timer.cnt,
    }
}

#[cfg(target_arch = "aarch64")]
fn replace_required_reg_u64(state: &mut VcpuState, id: u64, value: u64) -> Result<()> {
    replace_required_reg_bytes(state, id, value.to_le_bytes().to_vec())
}

#[cfg(target_arch = "aarch64")]
fn replace_required_reg_bytes(state: &mut VcpuState, id: u64, value: Vec<u8>) -> Result<()> {
    let Some(reg) = state.regs.iter_mut().find(|reg| reg.id == id) else {
        return Err(Error::VcpuUnhandledKvmExit);
    };
    if reg.value.len() != value.len() {
        return Err(Error::VcpuUnhandledKvmExit);
    }
    reg.value = value;
    Ok(())
}

#[cfg(target_arch = "aarch64")]
fn replace_optional_reg_u64(state: &mut VcpuState, id: u64, value: u64) -> Result<()> {
    replace_optional_reg_bytes(state, id, &value.to_le_bytes())
}

#[cfg(target_arch = "aarch64")]
fn replace_optional_reg_u32(state: &mut VcpuState, id: u64, value: u32) -> Result<()> {
    replace_optional_reg_bytes(state, id, &value.to_le_bytes())
}

#[cfg(target_arch = "aarch64")]
fn replace_optional_reg_u128(state: &mut VcpuState, id: u64, value: u128) -> Result<()> {
    replace_optional_reg_bytes(state, id, &value.to_le_bytes())
}

#[cfg(target_arch = "aarch64")]
fn replace_optional_reg_bytes(state: &mut VcpuState, id: u64, value: &[u8]) -> Result<()> {
    if let Some(reg) = state.regs.iter_mut().find(|reg| reg.id == id) {
        if reg.value.len() != value.len() {
            return Err(Error::VcpuUnhandledKvmExit);
        }
        reg.value.clear();
        reg.value.extend_from_slice(value);
    }
    Ok(())
}

#[cfg(target_arch = "aarch64")]
fn core_reg_id(offset: usize, size: u64) -> u64 {
    KVM_REG_ARM64 as u64
        | size
        | u64::from(KVM_REG_ARM_CORE)
        | (offset / std::mem::size_of::<u32>()) as u64
}

#[cfg(target_arch = "aarch64")]
fn core_user_reg_id(index: usize) -> u64 {
    let offset = std::mem::offset_of!(kvm_regs, regs)
        + std::mem::offset_of!(user_pt_regs, regs)
        + index * std::mem::size_of::<u64>();
    core_reg_id(offset, KVM_REG_SIZE_U64)
}

#[cfg(target_arch = "aarch64")]
fn core_user_sp_id() -> u64 {
    let offset = std::mem::offset_of!(kvm_regs, regs) + std::mem::offset_of!(user_pt_regs, sp);
    core_reg_id(offset, KVM_REG_SIZE_U64)
}

#[cfg(target_arch = "aarch64")]
fn core_user_pc_id() -> u64 {
    let offset = std::mem::offset_of!(kvm_regs, regs) + std::mem::offset_of!(user_pt_regs, pc);
    core_reg_id(offset, KVM_REG_SIZE_U64)
}

#[cfg(target_arch = "aarch64")]
fn core_user_pstate_id() -> u64 {
    let offset = std::mem::offset_of!(kvm_regs, regs) + std::mem::offset_of!(user_pt_regs, pstate);
    core_reg_id(offset, KVM_REG_SIZE_U64)
}

#[cfg(target_arch = "aarch64")]
fn core_sp_el1_id() -> u64 {
    core_reg_id(std::mem::offset_of!(kvm_regs, sp_el1), KVM_REG_SIZE_U64)
}

#[cfg(target_arch = "aarch64")]
fn core_elr_el1_id() -> u64 {
    core_reg_id(std::mem::offset_of!(kvm_regs, elr_el1), KVM_REG_SIZE_U64)
}

#[cfg(target_arch = "aarch64")]
fn core_spsr_id(index: usize) -> u64 {
    let offset = std::mem::offset_of!(kvm_regs, spsr) + index * std::mem::size_of::<u64>();
    core_reg_id(offset, KVM_REG_SIZE_U64)
}

#[cfg(target_arch = "aarch64")]
fn core_fp_vreg_id(index: usize) -> u64 {
    let offset = std::mem::offset_of!(kvm_regs, fp_regs)
        + std::mem::offset_of!(user_fpsimd_state, vregs)
        + index * std::mem::size_of::<u128>();
    core_reg_id(offset, KVM_REG_SIZE_U128)
}

#[cfg(target_arch = "aarch64")]
fn core_fp_fpsr_id() -> u64 {
    let offset =
        std::mem::offset_of!(kvm_regs, fp_regs) + std::mem::offset_of!(user_fpsimd_state, fpsr);
    core_reg_id(offset, KVM_REG_SIZE_U32)
}

#[cfg(target_arch = "aarch64")]
fn core_fp_fpcr_id() -> u64 {
    let offset =
        std::mem::offset_of!(kvm_regs, fp_regs) + std::mem::offset_of!(user_fpsimd_state, fpcr);
    core_reg_id(offset, KVM_REG_SIZE_U32)
}

#[cfg(target_arch = "aarch64")]
const fn hvf_sys_reg(op0: u16, op1: u16, crn: u16, crm: u16, op2: u16) -> u16 {
    (op0 << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2
}

#[cfg(target_arch = "aarch64")]
fn hvf_sysreg_is_host_derived(reg: u16) -> bool {
    (hvf_sysreg_op0(reg) == 3
        && hvf_sysreg_op1(reg) == 0
        && hvf_sysreg_crn(reg) == 0
        && hvf_sysreg_crm(reg) < 8)
        || reg == hvf_sys_reg(3, 4, 0, 0, 0)
        || reg == hvf_sys_reg(3, 4, 0, 0, 5)
        || reg == hvf_sys_reg(3, 0, 1, 0, 1)
        || reg == hvf_sys_reg(3, 4, 1, 0, 1)
        || reg == hvf_sys_reg(3, 0, 0, 0, 1)
        || reg == hvf_sys_reg(3, 0, 0, 0, 7)
        || reg == hvf_sys_reg(3, 2, 0, 0, 0)
        || reg == hvf_sys_reg(3, 3, 0, 0, 0)
        || reg == hvf_sys_reg(3, 3, 0, 0, 7)
        || (hvf_sysreg_op0(reg) == 3 && hvf_sysreg_op1(reg) == 1 && hvf_sysreg_crn(reg) == 15)
}

#[cfg(target_arch = "aarch64")]
fn hvf_sysreg_to_kvm_reg_id(reg: u16) -> u64 {
    arm64_sys_reg_id(
        u64::from(hvf_sysreg_op0(reg)),
        u64::from(hvf_sysreg_op1(reg)),
        u64::from(hvf_sysreg_crn(reg)),
        u64::from(hvf_sysreg_crm(reg)),
        u64::from(hvf_sysreg_op2(reg)),
    )
}

#[cfg(target_arch = "aarch64")]
fn hvf_sysreg_op0(reg: u16) -> u16 {
    (reg >> 14) & 0x3
}

#[cfg(target_arch = "aarch64")]
fn hvf_sysreg_op1(reg: u16) -> u16 {
    (reg >> 11) & 0x7
}

#[cfg(target_arch = "aarch64")]
fn hvf_sysreg_crn(reg: u16) -> u16 {
    (reg >> 7) & 0xf
}

#[cfg(target_arch = "aarch64")]
fn hvf_sysreg_crm(reg: u16) -> u16 {
    (reg >> 3) & 0xf
}

#[cfg(target_arch = "aarch64")]
fn hvf_sysreg_op2(reg: u16) -> u16 {
    reg & 0x7
}

#[cfg(target_arch = "aarch64")]
fn one_reg_size(reg_id: u64) -> Result<usize> {
    match reg_id & KVM_REG_SIZE_MASK {
        value if value == u64::from(KVM_REG_SIZE_U8) => Ok(1),
        KVM_REG_SIZE_U16 => Ok(2),
        KVM_REG_SIZE_U32 => Ok(4),
        KVM_REG_SIZE_U64 => Ok(8),
        KVM_REG_SIZE_U128 => Ok(16),
        KVM_REG_SIZE_U256 => Ok(32),
        KVM_REG_SIZE_U512 => Ok(64),
        KVM_REG_SIZE_U1024 => Ok(128),
        KVM_REG_SIZE_U2048 => Ok(256),
        _ => Err(Error::VcpuUnhandledKvmExit),
    }
}

#[cfg(target_arch = "aarch64")]
const TMR_CTL_ENABLE: u64 = 1;
#[cfg(target_arch = "aarch64")]
const TMR_CTL_IMASK: u64 = 1 << 1;
#[cfg(target_arch = "aarch64")]
const TMR_CTL_ISTATUS: u64 = 1 << 2;

#[cfg(target_arch = "x86_64")]
const MSR_IA32_TSC: u32 = 0x0000_0010;
#[cfg(target_arch = "x86_64")]
const MSR_IA32_TSC_DEADLINE: u32 = 0x0000_06e0;
#[cfg(target_arch = "x86_64")]
const DETERMINISTIC_X86_TSC_HZ: u64 = 1_000_000_000;

#[cfg(target_arch = "aarch64")]
const DETERMINISTIC_ARM_COUNTER_HZ: u64 = 1_000_000_000;

#[cfg(target_arch = "aarch64")]
static DETERMINISTIC_TIME_DEBUG_EVENTS: AtomicU64 = AtomicU64::new(0);

#[cfg(target_arch = "aarch64")]
fn deterministic_time_debug_event(message: String) {
    if std::env::var_os("KRUN_DETERMINISTIC_DEBUG").is_none() {
        return;
    }
    if DETERMINISTIC_TIME_DEBUG_EVENTS.fetch_add(1, Ordering::Relaxed) < 200 {
        crate::timing_event(&message);
    }
}

#[cfg(target_arch = "aarch64")]
fn arm64_sys_reg_id(op0: u64, op1: u64, crn: u64, crm: u64, op2: u64) -> u64 {
    KVM_REG_ARM64 as u64
        | KVM_REG_SIZE_U64 as u64
        | KVM_REG_ARM64_SYSREG as u64
        | ((op0 << KVM_REG_ARM64_SYSREG_OP0_SHIFT) & KVM_REG_ARM64_SYSREG_OP0_MASK as u64)
        | ((op1 << KVM_REG_ARM64_SYSREG_OP1_SHIFT) & KVM_REG_ARM64_SYSREG_OP1_MASK as u64)
        | ((crn << KVM_REG_ARM64_SYSREG_CRN_SHIFT) & KVM_REG_ARM64_SYSREG_CRN_MASK as u64)
        | ((crm << KVM_REG_ARM64_SYSREG_CRM_SHIFT) & KVM_REG_ARM64_SYSREG_CRM_MASK as u64)
        | ((op2 << KVM_REG_ARM64_SYSREG_OP2_SHIFT) & KVM_REG_ARM64_SYSREG_OP2_MASK as u64)
}

// Allow currently unused Pause and Exit events. These will be used by the vmm later on.
#[allow(unused)]
#[derive(Debug)]
/// List of events that the Vcpu can receive.
pub enum VcpuEvent {
    /// Pause the Vcpu.
    Pause,
    /// Event that should resume the Vcpu.
    Resume,
    /// Restore serialized vCPU state while paused.
    RestoreState(Vec<u8>),
    /// Restore serialized macOS HVF vCPU state while paused.
    RestoreHvfState {
        state: Vec<u8>,
        capture_counter: u64,
    },
    /// Re-arm virtual timer state after snapshot restore while paused.
    RebaseTimer(u64),
}

#[derive(Debug, Eq, PartialEq)]
/// List of responses that the Vcpu reports.
pub enum VcpuResponse {
    /// Vcpu is paused.
    Paused(Vec<u8>),
    /// Vcpu is resumed.
    Resumed,
    /// Serialized vCPU state was restored.
    Restored,
    /// Virtual timer state was re-armed.
    TimerRebased,
    /// Vcpu is stopped.
    Exited(u8),
    /// vCPU operation failed.
    Error(String),
}

/// Wrapper over Vcpu that hides the underlying interactions with the Vcpu thread.
pub struct VcpuHandle {
    event_sender: Sender<VcpuEvent>,
    response_receiver: Receiver<VcpuResponse>,
    // Rust JoinHandles have to be wrapped in Option if you ever plan on 'join()'ing them.
    // We want to be able to join these threads in tests.
    vcpu_thread: Option<thread::JoinHandle<()>>,
}

pub struct VcpuKicker {
    pthread: usize,
}

impl VcpuKicker {
    pub fn kick(&self) -> Result<()> {
        let pthread = self.pthread as libc::pthread_t;
        let rc = unsafe { libc::pthread_kill(pthread, sigrtmin() + VCPU_RTSIG_OFFSET) };
        if rc == 0 {
            Ok(())
        } else {
            Err(Error::SignalVcpu(utils::errno::Error::new(rc)))
        }
    }
}

impl VcpuHandle {
    pub fn new(
        event_sender: Sender<VcpuEvent>,
        response_receiver: Receiver<VcpuResponse>,
        vcpu_thread: thread::JoinHandle<()>,
    ) -> Self {
        Self {
            event_sender,
            response_receiver,
            vcpu_thread: Some(vcpu_thread),
        }
    }

    pub fn send_event(&self, event: VcpuEvent) -> Result<()> {
        // Use expect() to crash if the other thread closed this channel.
        self.event_sender
            .send(event)
            .expect("event sender channel closed on vcpu end.");
        // Kick the vcpu so it picks up the message.
        self.kick()
    }

    pub fn kick(&self) -> Result<()> {
        self.vcpu_thread
            .as_ref()
            // Safe to unwrap since constructor make this 'Some'.
            .unwrap()
            .kill(sigrtmin() + VCPU_RTSIG_OFFSET)
            .map_err(Error::SignalVcpu)?;
        Ok(())
    }

    pub fn try_clone_kicker(&self) -> Option<VcpuKicker> {
        self.vcpu_thread.as_ref().map(|handle| VcpuKicker {
            pthread: handle.as_pthread_t() as usize,
        })
    }

    pub fn response_receiver(&self) -> &Receiver<VcpuResponse> {
        &self.response_receiver
    }
}

enum VcpuEmulation {
    Handled,
    Interrupted,
    Stopped,
}

#[cfg(test)]
mod tests {
    use crossbeam_channel::unbounded;
    use std::sync::{Arc, Barrier};

    use super::*;
    #[cfg(target_arch = "aarch64")]
    use crate::builder::Payload;
    #[cfg(target_arch = "aarch64")]
    use crate::builder::create_guest_memory;
    #[cfg(target_arch = "aarch64")]
    use crate::resources::VmResources;
    use devices;
    #[cfg(target_arch = "x86_64")]
    use devices::legacy::KvmIoapic;

    use utils::signal::validate_signal_num;

    // In tests we need to close any pending Vcpu threads on test completion.
    impl Drop for VcpuHandle {
        fn drop(&mut self) {
            // Make sure the Vcpu is out of KVM_RUN.
            self.send_event(VcpuEvent::Pause).unwrap();
            // Close the original channel so that the Vcpu thread errors and goes to exit state.
            let (event_sender, _event_receiver) = unbounded();
            self.event_sender = event_sender;
            // Wait for the Vcpu thread to finish execution
            self.vcpu_thread.take().unwrap().join().unwrap();
        }
    }

    #[cfg(target_arch = "aarch64")]
    fn one_reg_u64(id: u64, value: u64) -> Aarch64OneReg {
        Aarch64OneReg {
            id,
            value: value.to_le_bytes().to_vec(),
        }
    }

    #[cfg(target_arch = "aarch64")]
    fn one_reg_u32(id: u64, value: u32) -> Aarch64OneReg {
        Aarch64OneReg {
            id,
            value: value.to_le_bytes().to_vec(),
        }
    }

    #[cfg(target_arch = "aarch64")]
    fn one_reg_u128(id: u64, value: u128) -> Aarch64OneReg {
        Aarch64OneReg {
            id,
            value: value.to_le_bytes().to_vec(),
        }
    }

    #[cfg(target_arch = "aarch64")]
    fn reg_bytes(state: &VcpuState, id: u64) -> &[u8] {
        state
            .regs
            .iter()
            .find(|reg| reg.id == id)
            .map(|reg| reg.value.as_slice())
            .expect("missing register")
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn hvf_state_translation_maps_core_fp_and_sysregs() {
        let writable_sysreg = hvf_sys_reg(3, 0, 1, 0, 0);
        let readonly_id_sysreg = hvf_sys_reg(3, 0, 0, 1, 0);
        let readonly_el2_id_sysreg = hvf_sys_reg(3, 4, 0, 0, 5);
        let host_derived_sysreg = hvf_sys_reg(3, 0, 1, 0, 1);
        let hvf_cntv_cval = hvf_sys_reg(3, 3, 14, 3, 2);
        let kvm_timer_cval = kvm_timer_cval_id();
        let kvm_timer_counter = kvm_timer_counter_id();
        let mut regs = Vec::new();
        for index in 0..31 {
            regs.push(one_reg_u64(core_user_reg_id(index), 0));
        }
        regs.extend([
            one_reg_u64(core_user_sp_id(), 0),
            one_reg_u64(core_user_pc_id(), 0),
            one_reg_u64(core_user_pstate_id(), 0),
            one_reg_u64(core_spsr_id(0), 0),
            one_reg_u64(core_elr_el1_id(), 0),
            one_reg_u64(core_sp_el1_id(), 0),
            one_reg_u32(core_fp_fpsr_id(), 0),
            one_reg_u32(core_fp_fpcr_id(), 0),
            one_reg_u128(core_fp_vreg_id(0), 0),
            one_reg_u64(hvf_sysreg_to_kvm_reg_id(writable_sysreg), 0),
            one_reg_u64(hvf_sysreg_to_kvm_reg_id(readonly_id_sysreg), 0xfeed),
            one_reg_u64(hvf_sysreg_to_kvm_reg_id(readonly_el2_id_sysreg), 0xbeef),
            one_reg_u64(hvf_sysreg_to_kvm_reg_id(host_derived_sysreg), 0xcafe),
            one_reg_u64(kvm_timer_cval, 0),
            one_reg_u64(kvm_timer_counter, 0x9999),
        ]);
        let mut state = VcpuState {
            regs,
            mp_state: None,
            vcpu_events: None,
        };
        let mut hvf = HvfVcpuStateCompat {
            gp: [0; 31],
            pc: 0x3000,
            cpsr: 0x3c5,
            fpcr: 0x44,
            fpsr: 0x55,
            fp: [0; 32],
            sysregs: vec![
                (writable_sysreg, 0x1111),
                (readonly_id_sysreg, 0x2222),
                (readonly_el2_id_sysreg, 0xaaaa),
                (host_derived_sysreg, 0xbbbb),
                (hvf_sys_reg(3, 0, 4, 0, 0), 0x3333),
                (hvf_sys_reg(3, 0, 4, 0, 1), 0x4444),
                (hvf_sys_reg(3, 0, 4, 1, 0), 0x5555),
                (hvf_sys_reg(3, 4, 4, 1, 0), 0x6666),
                (hvf_cntv_cval, 0x8888),
            ],
            gic_icc_regs: vec![(hvf_sys_reg(3, 0, 4, 6, 0), 0xf0)],
            gic_redist_regs: Vec::new(),
            gic_ich_regs: Vec::new(),
            vtimer_masked: false,
            vtimer_offset: 0x1234,
        };
        hvf.gp[0] = 0x1000;
        hvf.gp[30] = 0x2000;
        hvf.fp[0] = 0x7777_8888_9999_aaaa_bbbb_cccc_dddd_eeee;

        merge_hvf_state_into_kvm_state(&mut state, &hvf, 0x9000).expect("translate");

        assert_eq!(
            reg_bytes(&state, core_user_reg_id(0)),
            &0x1000u64.to_le_bytes()
        );
        assert_eq!(
            reg_bytes(&state, core_user_reg_id(30)),
            &0x2000u64.to_le_bytes()
        );
        assert_eq!(
            reg_bytes(&state, core_user_pc_id()),
            &0x3000u64.to_le_bytes()
        );
        assert_eq!(
            reg_bytes(&state, core_user_pstate_id()),
            &0x3c5u64.to_le_bytes()
        );
        assert_eq!(reg_bytes(&state, core_fp_fpcr_id()), &0x44u32.to_le_bytes());
        assert_eq!(reg_bytes(&state, core_fp_fpsr_id()), &0x55u32.to_le_bytes());
        assert_eq!(
            reg_bytes(&state, core_fp_vreg_id(0)),
            &0x7777_8888_9999_aaaa_bbbb_cccc_dddd_eeeeu128.to_le_bytes()
        );
        assert_eq!(
            reg_bytes(&state, hvf_sysreg_to_kvm_reg_id(writable_sysreg)),
            &0x1111u64.to_le_bytes()
        );
        assert_eq!(
            reg_bytes(&state, hvf_sysreg_to_kvm_reg_id(readonly_id_sysreg)),
            &0xfeedu64.to_le_bytes()
        );
        assert_eq!(
            reg_bytes(&state, hvf_sysreg_to_kvm_reg_id(readonly_el2_id_sysreg)),
            &0xbeefu64.to_le_bytes()
        );
        assert_eq!(
            reg_bytes(&state, hvf_sysreg_to_kvm_reg_id(host_derived_sysreg)),
            &0xcafeu64.to_le_bytes()
        );
        assert_eq!(reg_bytes(&state, core_spsr_id(0)), &0x3333u64.to_le_bytes());
        assert_eq!(
            reg_bytes(&state, core_elr_el1_id()),
            &0x4444u64.to_le_bytes()
        );
        assert_eq!(
            reg_bytes(&state, core_user_sp_id()),
            &0x5555u64.to_le_bytes()
        );
        assert_eq!(
            reg_bytes(&state, core_sp_el1_id()),
            &0x6666u64.to_le_bytes()
        );
        assert_eq!(reg_bytes(&state, kvm_timer_cval), &0x8888u64.to_le_bytes());
        assert_eq!(
            reg_bytes(&state, kvm_timer_counter),
            &0x7dccu64.to_le_bytes()
        );
        assert_eq!(
            state.mp_state.map(|state| state.mp_state),
            Some(KVM_MP_STATE_RUNNABLE)
        );
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn hvf_control_sysreg_sanitizer_masks_kvm_host_normalized_bits() {
        assert_eq!(
            sanitize_hvf_control_sysreg_value(hvf_sys_reg(3, 0, 1, 0, 0), 0x20000183474d99d),
            0x3474d99d
        );
        assert_eq!(
            sanitize_hvf_control_sysreg_value(hvf_sys_reg(3, 0, 2, 0, 2), 0x51000727551b511),
            0x727551b511
        );
        assert_eq!(
            sanitize_hvf_control_sysreg_value(hvf_sys_reg(3, 0, 12, 0, 0), 0xffffc00080010800),
            0xffffc00080010800
        );
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn hvf_state_translation_requires_core_registers() {
        let mut state = VcpuState {
            regs: vec![one_reg_u64(core_user_pc_id(), 0)],
            mp_state: None,
            vcpu_events: None,
        };
        let hvf = HvfVcpuStateCompat {
            gp: [0; 31],
            pc: 0,
            cpsr: 0,
            fpcr: 0,
            fpsr: 0,
            fp: [0; 32],
            sysregs: Vec::new(),
            gic_icc_regs: Vec::new(),
            gic_redist_regs: Vec::new(),
            gic_ich_regs: Vec::new(),
            vtimer_masked: false,
            vtimer_offset: 0,
        };

        assert!(merge_hvf_state_into_kvm_state(&mut state, &hvf, 0).is_err());
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn hvf_timer_rearm_moves_expired_comparator_and_clears_pending_status() {
        let timer = HvfTimerRestoreState {
            ctl: Some(TMR_CTL_ENABLE | TMR_CTL_ISTATUS),
            cval: Some(0x1000),
            cnt: Some(0x1001),
        };

        let rearmed = rearmed_hvf_timer_state(timer);

        assert_eq!(rearmed.cval, Some(0x1001 + HVF_EXPIRED_TIMER_REARM_TICKS));
        assert_eq!(rearmed.ctl, Some(TMR_CTL_ENABLE));
        assert_eq!(rearmed.cnt, Some(0x1001));
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn hvf_timer_rearm_moves_near_deadline_before_it_expires() {
        let timer = HvfTimerRestoreState {
            ctl: Some(TMR_CTL_ENABLE),
            cval: Some(0x1001 + HVF_EXPIRED_TIMER_REARM_TICKS - 1),
            cnt: Some(0x1001),
        };

        let rearmed = rearmed_hvf_timer_state(timer);

        assert_eq!(rearmed.cval, Some(0x1001 + HVF_EXPIRED_TIMER_REARM_TICKS));
        assert_eq!(rearmed.ctl, Some(TMR_CTL_ENABLE));
        assert_eq!(rearmed.cnt, Some(0x1001));
    }

    #[test]
    fn deterministic_timer_jump_updates_clock_state_file() {
        let path = std::env::temp_dir().join(format!(
            "lnx-deterministic-clock-state-{}-{}.state",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_file(&path);
        std::fs::write(
            &path,
            "clock_state=deterministic-clock-state-v1\nrealtime_unix_nanos=0\nmonotonic_nanos=0\ncounter_frequency_hz=1000000000\nevent_sequence=3\ntimer_jump_count=4\nlast_timer_deadline_ticks=5\n",
        )
        .expect("write clock state");

        update_deterministic_clock_state_file(&path, 99, 1_000_000_000)
            .expect("update clock state");

        let updated = std::fs::read_to_string(&path).expect("read clock state");
        assert!(updated.contains("realtime_unix_nanos=99\n"));
        assert!(updated.contains("monotonic_nanos=99\n"));
        assert!(updated.contains("counter_frequency_hz=1000000000\n"));
        assert!(updated.contains("event_sequence=3\n"));
        assert!(updated.contains("timer_jump_count=5\n"));
        assert!(updated.contains("last_timer_deadline_ticks=99\n"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn deterministic_timer_jump_rejects_counter_frequency_mismatch() {
        let path = std::env::temp_dir().join(format!(
            "lnx-deterministic-clock-state-frequency-{}.state",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        std::fs::write(
            &path,
            "clock_state=deterministic-clock-state-v1\nrealtime_unix_nanos=42\nmonotonic_nanos=42\ncounter_frequency_hz=1000000000\nevent_sequence=0\ntimer_jump_count=1\nlast_timer_deadline_ticks=42\n",
        )
        .expect("write clock state");

        let err = update_deterministic_clock_state_file(&path, 1, 24_000_000)
            .expect_err("frequency mismatch");

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn deterministic_timer_jump_requires_writable_clock_state_file() {
        let path = std::env::temp_dir().join(format!(
            "lnx-deterministic-clock-state-missing-{}.state",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let err = update_deterministic_clock_state_file(&path, 1, 1_000_000_000)
            .expect_err("missing clock state");

        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    // Auxiliary function being used throughout the tests.
    fn setup_vcpu(mem_size: usize) -> (Vm, Vcpu, GuestMemoryMmap) {
        let kvm = KvmContext::new().unwrap();
        let gm = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), mem_size)]).unwrap();
        let mut vm = Vm::new(kvm.fd()).expect("Cannot create new vm");
        #[cfg(target_arch = "x86_64")]
        let _kvmioapic = KvmIoapic::new(vm.fd()).unwrap();
        assert!(vm.memory_init(&gm, kvm.max_memslots()).is_ok());

        let exit_evt = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();

        let vcpu;
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            vcpu = Vcpu::new_x86_64(
                1,
                vm.fd(),
                vm.supported_cpuid().clone(),
                vm.supported_msrs().clone(),
                devices::Bus::new(),
                exit_evt,
            )
            .unwrap();
        }
        #[cfg(target_arch = "aarch64")]
        {
            vcpu = Vcpu::new_aarch64(1, vm.fd(), exit_evt).unwrap();
        }

        (vm, vcpu, gm)
    }

    #[test]
    fn test_set_mmio_bus() {
        let (_, mut vcpu, _) = setup_vcpu(0x1000);
        assert!(vcpu.mmio_bus.is_none());
        vcpu.set_mmio_bus(devices::Bus::new());
        assert!(vcpu.mmio_bus.is_some());
    }

    #[ignore]
    #[test]
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    fn test_get_supported_cpuid() {
        let kvm = KvmContext::new().unwrap();
        let vm = Vm::new(kvm.fd()).expect("Cannot create new vm");
        let cpuid = kvm
            .kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .expect("Cannot get supported cpuid");
        assert_eq!(vm.supported_cpuid().as_slice(), cpuid.as_slice());
    }

    #[test]
    fn test_vm_memory_init() {
        let mut kvm_context = KvmContext::new().unwrap();
        let mut vm = Vm::new(kvm_context.fd()).expect("Cannot create new vm");

        // Create valid memory region and test that the initialization is successful.
        let gm = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1000)]).unwrap();
        assert!(vm.memory_init(&gm, kvm_context.max_memslots()).is_ok());

        // Set the maximum number of memory slots to 1 in KvmContext to check the error
        // path of memory_init. Create 2 non-overlapping memory slots.
        kvm_context.max_memslots = 1;
        let gm = GuestMemoryMmap::from_ranges(&[
            (GuestAddress(0x0), 0x1000),
            (GuestAddress(0x1001), 0x2000),
        ])
        .unwrap();
        assert!(vm.memory_init(&gm, kvm_context.max_memslots()).is_err());
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_configure_vcpu() {
        let (_vm, mut vcpu, vm_mem) = setup_vcpu(0x10000);

        let mut vcpu_config = VcpuConfig {
            vcpu_count: 1,
            ht_enabled: false,
            cpu_template: None,
            nested_enabled: false,
        };

        assert!(
            vcpu.configure_x86_64(&vm_mem, GuestAddress(0), &vcpu_config, true, false)
                .is_ok()
        );

        // Test configure while using the T2 template.
        vcpu_config.cpu_template = Some(CpuFeaturesTemplate::T2);
        assert!(
            vcpu.configure_x86_64(&vm_mem, GuestAddress(0), &vcpu_config, true, false)
                .is_ok()
        );

        // Test configure while using the C3 template.
        vcpu_config.cpu_template = Some(CpuFeaturesTemplate::C3);
        assert!(
            vcpu.configure_x86_64(&vm_mem, GuestAddress(0), &vcpu_config, true, false)
                .is_ok()
        );
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn test_configure_vcpu() {
        let kvm = KvmContext::new().unwrap();
        let vm_resources = VmResources::default();
        let (guest_memory, arch_memory_info, _shm_manager, _payload_config, _pmem_regions) =
            create_guest_memory(128, &vm_resources, &Payload::Empty).unwrap();
        let mut vm = Vm::new(kvm.fd()).expect("new vm failed");
        assert!(vm.memory_init(&guest_memory, kvm.max_memslots()).is_ok());

        // Try it for when vcpu id is 0.
        let mut vcpu = Vcpu::new_aarch64(
            0,
            vm.fd(),
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
        )
        .unwrap();

        assert!(
            vcpu.configure_aarch64(vm.fd(), &arch_memory_info, GuestAddress(0))
                .is_ok()
        );

        // Try it for when vcpu id is NOT 0.
        let mut vcpu = Vcpu::new_aarch64(
            1,
            vm.fd(),
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
        )
        .unwrap();

        assert!(
            vcpu.configure_aarch64(vm.fd(), &arch_memory_info, GuestAddress(0))
                .is_ok()
        );
    }

    #[test]
    fn test_vcpu_tls() {
        let (_, mut vcpu, _) = setup_vcpu(0x1000);

        // Running on the TLS vcpu should fail before we actually initialize it.
        unsafe {
            assert!(Vcpu::run_on_thread_local(|_| ()).is_err());
        }

        // Initialize vcpu TLS.
        vcpu.init_thread_local_data().unwrap();

        // Validate TLS vcpu is the local vcpu by changing the `id` then validating against
        // the one in TLS.
        vcpu.id = 12;
        unsafe {
            assert!(Vcpu::run_on_thread_local(|v| assert_eq!(v.id, 12)).is_ok());
        }

        // Reset vcpu TLS.
        assert!(vcpu.reset_thread_local_data().is_ok());

        // Running on the TLS vcpu after TLS reset should fail.
        unsafe {
            assert!(Vcpu::run_on_thread_local(|_| ()).is_err());
        }

        // Second reset should return error.
        assert!(vcpu.reset_thread_local_data().is_err());
    }

    #[test]
    fn test_invalid_tls() {
        let (_, mut vcpu, _) = setup_vcpu(0x1000);
        // Initialize vcpu TLS.
        vcpu.init_thread_local_data().unwrap();
        // Trying to initialize non-empty TLS should error.
        vcpu.init_thread_local_data().unwrap_err();
    }

    #[test]
    fn test_vcpu_kick() {
        Vcpu::register_kick_signal_handler();
        let (vm, mut vcpu, _mem) = setup_vcpu(0x1000);

        let mut kvm_run =
            KvmRunWrapper::mmap_from_fd(&vcpu.fd, vm.fd.run_size()).expect("cannot mmap kvm-run");
        let success = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let vcpu_success = success.clone();
        let barrier = Arc::new(Barrier::new(2));
        let vcpu_barrier = barrier.clone();
        // Start Vcpu thread which will be kicked with a signal.
        let handle = std::thread::Builder::new()
            .name("test_vcpu_kick".to_string())
            .spawn(move || {
                vcpu.init_thread_local_data().unwrap();
                // Notify TLS was populated.
                vcpu_barrier.wait();
                // Loop for max 1 second to check if the signal handler has run.
                for _ in 0..10 {
                    if kvm_run.as_mut_ref().immediate_exit == 1 {
                        // Signal handler has run and set immediate_exit to 1.
                        vcpu_success.store(true, Ordering::Release);
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            })
            .expect("cannot start thread");

        // Wait for the vcpu to initialize its TLS.
        barrier.wait();
        // Kick the Vcpu using the custom signal.
        handle
            .kill(sigrtmin() + VCPU_RTSIG_OFFSET)
            .expect("failed to signal thread");
        handle.join().expect("failed to join thread");
        // Verify that the Vcpu saw its kvm immediate-exit as set.
        assert!(success.load(Ordering::Acquire));
    }

    #[test]
    fn test_vcpu_rtsig_offset() {
        assert!(validate_signal_num(sigrtmin() + VCPU_RTSIG_OFFSET).is_ok());
    }
}
