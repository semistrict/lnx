// vCPU state capture/restore for HVF.
//
// Captures the architectural state the guest is allowed to observe so the same
// guest can be resumed on the same host: GP regs, FP/SIMD regs, the set of
// system regs HVF lets us read, and a few flags from the HvfVcpu wrapper.

use serde::{Deserialize, Serialize};

use crate::bindings::*;
use crate::{Error, HvfVcpu, vcpu_set_vtimer_mask};

const TMR_CTL_ENABLE: u64 = 1 << 0;
const TMR_CTL_ISTATUS: u64 = 1 << 2;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
unsafe extern "C" {
    fn krun_hv_vcpu_set_simd_fp_reg_from_bytes(
        vcpu: hv_vcpu_t,
        reg: hv_simd_fp_reg_t,
        value_bytes: *const u8,
    ) -> hv_return_t;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HvfVcpuState {
    /// X0..X30 (HVF numbers 0..30), then PC (HVF reg 31), then CPSR.
    pub gp: [u64; 31],
    pub pc: u64,
    pub cpsr: u64,
    pub fpcr: u64,
    pub fpsr: u64,
    /// Q0..Q31, 128 bits each.
    pub fp: [u128; 32],
    /// (hv_sys_reg_t, value). Order is the order of capture.
    pub sysregs: Vec<(u16, u64)>,
    /// GIC CPU-interface registers. The opaque HVF GIC state blob explicitly
    /// excludes these, and the APIs must be called from the owning vCPU thread.
    #[serde(default)]
    pub gic_icc_regs: Vec<(u16, u64)>,
    /// GIC redistributor registers that are tied to this vCPU and must be read
    /// from the owning vCPU thread.
    #[serde(default)]
    pub gic_redist_regs: Vec<(u32, u64)>,
    /// GIC virtualization-control registers. These only exist when EL2 is
    /// enabled and are required for nested-guest interrupt injection state.
    #[serde(default)]
    pub gic_ich_regs: Vec<(u16, u64)>,
    pub vtimer_masked: bool,
    /// `hv_vcpu_get_vtimer_offset` at capture. This is HVF's userspace-accessible
    /// equivalent of CNTVOFF_EL2 — restoring it freezes the guest's view of
    /// CNTVCT_EL0 across the pause, which is essential to avoid the kernel
    /// hrtimer catch-up storm on restore.
    #[serde(default)]
    pub vtimer_offset: u64,
}

/// EL1 sysregs HVF exposes (all non-debug-breakpoint ones).
/// `hv_sys_reg_t` is a u16 enum.
pub const SYS_REGS_EL1: &[u16] = &[
    hv_sys_reg_t_HV_SYS_REG_MDCCINT_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_MDSCR_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_MIDR_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_MPIDR_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_ID_AA64PFR0_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_ID_AA64PFR1_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_ID_AA64DFR0_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_ID_AA64DFR1_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_ID_AA64ISAR0_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_ID_AA64ISAR1_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_ID_AA64MMFR0_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_ID_AA64MMFR1_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_ID_AA64MMFR2_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_SCTLR_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_ACTLR_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_CPACR_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_TTBR0_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_TTBR1_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_TCR_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_APIAKEYLO_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_APIAKEYHI_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_APIBKEYLO_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_APIBKEYHI_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_APDAKEYLO_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_APDAKEYHI_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_APDBKEYLO_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_APDBKEYHI_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_APGAKEYLO_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_APGAKEYHI_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_SPSR_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_ELR_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_SP_EL0 as u16,
    hv_sys_reg_t_HV_SYS_REG_AFSR0_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_AFSR1_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_ESR_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_FAR_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_PAR_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_MAIR_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_AMAIR_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_VBAR_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_CONTEXTIDR_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_TPIDR_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_CNTKCTL_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_CSSELR_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_TPIDR_EL0 as u16,
    hv_sys_reg_t_HV_SYS_REG_TPIDRRO_EL0 as u16,
    hv_sys_reg_t_HV_SYS_REG_CNTV_CTL_EL0 as u16,
    hv_sys_reg_t_HV_SYS_REG_CNTV_CVAL_EL0 as u16,
    hv_sys_reg_t_HV_SYS_REG_SP_EL1 as u16,
];

/// Additional sysregs only valid when EL2/nested is enabled.
pub const SYS_REGS_EL2: &[u16] = &[
    hv_sys_reg_t_HV_SYS_REG_CNTHCTL_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_CNTHP_CTL_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_CNTHP_CVAL_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_CNTVOFF_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_CPTR_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_ELR_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_ESR_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_FAR_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_HCR_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_HPFAR_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_MAIR_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_MDCR_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_SCTLR_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_SPSR_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_SP_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_TCR_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_TPIDR_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_TTBR0_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_TTBR1_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_VBAR_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_VMPIDR_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_VPIDR_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_VTCR_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_VTTBR_EL2 as u16,
];

const GIC_ICC_REGS: &[u16] = &[
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_SRE_EL1 as u16,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_CTLR_EL1 as u16,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_IGRPEN0_EL1 as u16,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_IGRPEN1_EL1 as u16,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_PMR_EL1 as u16,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_BPR0_EL1 as u16,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_BPR1_EL1 as u16,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_AP0R0_EL1 as u16,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_AP0R0_EL1 as u16 + 1,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_AP0R0_EL1 as u16 + 2,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_AP0R0_EL1 as u16 + 3,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_AP1R0_EL1 as u16,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_AP1R0_EL1 as u16 + 1,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_AP1R0_EL1 as u16 + 2,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_AP1R0_EL1 as u16 + 3,
];

const GIC_REDIST_REGS: &[u32] = &[
    hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_IGROUPR0 as u32,
    hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_ISENABLER0 as u32,
    hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_ICFGR0 as u32,
    hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_ICFGR1 as u32,
    hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_ISPENDR0 as u32,
    hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_ISACTIVER0 as u32,
    hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_IPRIORITYR0 as u32,
    hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_IPRIORITYR1 as u32,
    hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_IPRIORITYR2 as u32,
    hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_IPRIORITYR3 as u32,
    hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_IPRIORITYR4 as u32,
    hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_IPRIORITYR5 as u32,
    hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_IPRIORITYR6 as u32,
    hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_IPRIORITYR7 as u32,
];

const GIC_ICH_REGS: &[u16] = &[
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_VMCR_EL2 as u16,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_HCR_EL2 as u16,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR0_EL2 as u16,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR1_EL2 as u16,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR2_EL2 as u16,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR3_EL2 as u16,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR4_EL2 as u16,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR5_EL2 as u16,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR6_EL2 as u16,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR7_EL2 as u16,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR8_EL2 as u16,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR9_EL2 as u16,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR10_EL2 as u16,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR11_EL2 as u16,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR12_EL2 as u16,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR13_EL2 as u16,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR14_EL2 as u16,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR15_EL2 as u16,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_AP0R0_EL2 as u16,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_AP0R0_EL2 as u16 + 1,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_AP0R0_EL2 as u16 + 2,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_AP0R0_EL2 as u16 + 3,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_AP1R0_EL2 as u16,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_AP1R0_EL2 as u16 + 1,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_AP1R0_EL2 as u16 + 2,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_AP1R0_EL2 as u16 + 3,
];

/// Read-only ID/identification regs we capture for verification but do not write back
/// on restore (HVF rejects writes outside the nested-init window). Mac→Mac same-host
/// restore makes this safe: the host's values will match what was captured.
const RO_SYS_REGS: &[u16] = &[
    hv_sys_reg_t_HV_SYS_REG_MIDR_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_MPIDR_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_ID_AA64PFR0_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_ID_AA64PFR1_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_ID_AA64DFR0_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_ID_AA64DFR1_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_ID_AA64ISAR0_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_ID_AA64ISAR1_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_ID_AA64MMFR0_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_ID_AA64MMFR1_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_ID_AA64MMFR2_EL1 as u16,
    hv_sys_reg_t_HV_SYS_REG_VMPIDR_EL2 as u16,
    hv_sys_reg_t_HV_SYS_REG_VPIDR_EL2 as u16,
];

fn is_ro(reg: u16) -> bool {
    RO_SYS_REGS.contains(&reg)
}

fn is_blocked_timer_reg(reg: u16) -> bool {
    matches!(
        reg,
        x if x == hv_sys_reg_t_HV_SYS_REG_CNTP_CTL_EL0 as u16
            || x == hv_sys_reg_t_HV_SYS_REG_CNTP_CVAL_EL0 as u16
    )
}

fn restore_sys_reg(vcpuid: hv_vcpu_t, reg: u16, val: u64) {
    if is_ro(reg) || is_blocked_timer_reg(reg) {
        return;
    }
    let val = if matches!(
        reg,
        x if x == hv_sys_reg_t_HV_SYS_REG_CNTV_CTL_EL0 as u16
            || x == hv_sys_reg_t_HV_SYS_REG_CNTHP_CTL_EL2 as u16
    ) && (val & TMR_CTL_ENABLE) == 0
    {
        val & !TMR_CTL_ISTATUS
    } else {
        val
    };
    if let Err(e) = HvfVcpu::raw_set_sys_reg(vcpuid, reg, val) {
        debug!("snapshot restore: skipping sysreg 0x{reg:x}: {e}");
    }
}

fn is_el2_control_reg(reg: u16) -> bool {
    matches!(
        reg,
        x if x == hv_sys_reg_t_HV_SYS_REG_HCR_EL2 as u16
            || x == hv_sys_reg_t_HV_SYS_REG_SCTLR_EL2 as u16
            || x == hv_sys_reg_t_HV_SYS_REG_TCR_EL2 as u16
            || x == hv_sys_reg_t_HV_SYS_REG_TTBR0_EL2 as u16
            || x == hv_sys_reg_t_HV_SYS_REG_TTBR1_EL2 as u16
            || x == hv_sys_reg_t_HV_SYS_REG_MAIR_EL2 as u16
            || x == hv_sys_reg_t_HV_SYS_REG_VTCR_EL2 as u16
            || x == hv_sys_reg_t_HV_SYS_REG_VTTBR_EL2 as u16
    )
}

impl HvfVcpu<'_> {
    /// Read one GP register (X0..X30, PC, FPCR, FPSR, CPSR).
    fn raw_get_reg(vcpuid: hv_vcpu_t, reg: hv_reg_t) -> Result<u64, Error> {
        let val: u64 = 0;
        let ret = unsafe { hv_vcpu_get_reg(vcpuid, reg, &val as *const _ as *mut _) };
        if ret != HV_SUCCESS {
            Err(Error::VcpuReadRegister)
        } else {
            Ok(val)
        }
    }

    fn raw_set_reg(vcpuid: hv_vcpu_t, reg: hv_reg_t, value: u64) -> Result<(), Error> {
        let ret = unsafe { hv_vcpu_set_reg(vcpuid, reg, value) };
        if ret != HV_SUCCESS {
            Err(Error::VcpuSetRegister)
        } else {
            Ok(())
        }
    }

    fn raw_get_sys_reg(vcpuid: hv_vcpu_t, reg: u16) -> Result<u64, Error> {
        let val: u64 = 0;
        let ret =
            unsafe { hv_vcpu_get_sys_reg(vcpuid, reg as hv_sys_reg_t, &val as *const _ as *mut _) };
        if ret != HV_SUCCESS {
            Err(Error::VcpuReadSystemRegister)
        } else {
            Ok(val)
        }
    }

    fn raw_get_gic_icc_reg(vcpuid: hv_vcpu_t, reg: u16) -> Option<u64> {
        let mut val: u64 = 0;
        let ret = unsafe { hv_gic_get_icc_reg(vcpuid, reg as hv_gic_icc_reg_t, &mut val) };
        (ret == HV_SUCCESS).then_some(val)
    }

    fn raw_set_gic_icc_reg(vcpuid: hv_vcpu_t, reg: u16, value: u64) -> bool {
        let ret = unsafe { hv_gic_set_icc_reg(vcpuid, reg as hv_gic_icc_reg_t, value) };
        ret == HV_SUCCESS
    }

    fn raw_get_gic_redist_reg(vcpuid: hv_vcpu_t, reg: u32) -> Option<u64> {
        let mut val: u64 = 0;
        let ret = unsafe {
            hv_gic_get_redistributor_reg(vcpuid, reg as hv_gic_redistributor_reg_t, &mut val)
        };
        (ret == HV_SUCCESS).then_some(val)
    }

    fn raw_set_gic_redist_reg(vcpuid: hv_vcpu_t, reg: u32, value: u64) -> bool {
        let ret = unsafe {
            hv_gic_set_redistributor_reg(vcpuid, reg as hv_gic_redistributor_reg_t, value)
        };
        ret == HV_SUCCESS
    }

    fn raw_get_gic_ich_reg(vcpuid: hv_vcpu_t, reg: u16) -> Option<u64> {
        let mut val: u64 = 0;
        let ret = unsafe { hv_gic_get_ich_reg(vcpuid, reg as hv_gic_ich_reg_t, &mut val) };
        (ret == HV_SUCCESS).then_some(val)
    }

    fn raw_set_gic_ich_reg(vcpuid: hv_vcpu_t, reg: u16, value: u64) -> bool {
        let ret = unsafe { hv_gic_set_ich_reg(vcpuid, reg as hv_gic_ich_reg_t, value) };
        ret == HV_SUCCESS
    }

    fn restore_gic_redist_reg(vcpuid: hv_vcpu_t, reg: u32, value: u64) {
        if reg == hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_ISENABLER0 as u32 {
            let _ = Self::raw_set_gic_redist_reg(
                vcpuid,
                hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_ICENABLER0 as u32,
                u32::MAX as u64,
            );
        } else if reg == hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_ISPENDR0 as u32 {
            let _ = Self::raw_set_gic_redist_reg(
                vcpuid,
                hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_ICPENDR0 as u32,
                u32::MAX as u64,
            );
        } else if reg == hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_ISACTIVER0 as u32
        {
            let _ = Self::raw_set_gic_redist_reg(
                vcpuid,
                hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_ICACTIVER0 as u32,
                u32::MAX as u64,
            );
        }
        let _ = Self::raw_set_gic_redist_reg(vcpuid, reg, value);
    }

    fn restore_gic_redist_exact(vcpuid: hv_vcpu_t, regs: &[(u32, u64)], wanted: u32) {
        for &(reg, val) in regs {
            if reg == wanted {
                Self::restore_gic_redist_reg(vcpuid, reg, val);
            }
        }
    }

    fn restore_gic_redist_range(vcpuid: hv_vcpu_t, regs: &[(u32, u64)], base: u32, len: u32) {
        for &(reg, val) in regs {
            if (base..base + len).contains(&reg) {
                Self::restore_gic_redist_reg(vcpuid, reg, val);
            }
        }
    }

    fn is_ordered_gic_redist_reg(reg: u32) -> bool {
        let priority_base =
            hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_IPRIORITYR0 as u32;
        matches!(
            reg,
            x if x == hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_IGROUPR0 as u32
                || x == hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_ISENABLER0 as u32
                || x == hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_ICFGR0 as u32
                || x == hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_ICFGR1 as u32
                || x == hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_ISPENDR0 as u32
                || x == hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_ISACTIVER0 as u32
                || (priority_base..priority_base + 32).contains(&x)
        )
    }

    fn restore_gic_icc_reg(vcpuid: hv_vcpu_t, reg: u16, value: u64) {
        if !Self::raw_set_gic_icc_reg(vcpuid, reg, value) {
            debug!("snapshot restore: skipping GIC ICC reg 0x{reg:x}");
        }
    }

    fn restore_gic_icc_exact(vcpuid: hv_vcpu_t, regs: &[(u16, u64)], wanted: u16) {
        for &(reg, val) in regs {
            if reg == wanted {
                Self::restore_gic_icc_reg(vcpuid, reg, val);
            }
        }
    }

    fn restore_gic_icc_regs(vcpuid: hv_vcpu_t, regs: &[(u16, u64)]) {
        let ordered = [
            hv_gic_icc_reg_t_HV_GIC_ICC_REG_SRE_EL1 as u16,
            hv_gic_icc_reg_t_HV_GIC_ICC_REG_CTLR_EL1 as u16,
            hv_gic_icc_reg_t_HV_GIC_ICC_REG_IGRPEN0_EL1 as u16,
            hv_gic_icc_reg_t_HV_GIC_ICC_REG_IGRPEN1_EL1 as u16,
            hv_gic_icc_reg_t_HV_GIC_ICC_REG_PMR_EL1 as u16,
            hv_gic_icc_reg_t_HV_GIC_ICC_REG_BPR0_EL1 as u16,
            hv_gic_icc_reg_t_HV_GIC_ICC_REG_BPR1_EL1 as u16,
            hv_gic_icc_reg_t_HV_GIC_ICC_REG_AP0R0_EL1 as u16 + 3,
            hv_gic_icc_reg_t_HV_GIC_ICC_REG_AP0R0_EL1 as u16 + 2,
            hv_gic_icc_reg_t_HV_GIC_ICC_REG_AP0R0_EL1 as u16 + 1,
            hv_gic_icc_reg_t_HV_GIC_ICC_REG_AP0R0_EL1 as u16,
            hv_gic_icc_reg_t_HV_GIC_ICC_REG_AP1R0_EL1 as u16 + 3,
            hv_gic_icc_reg_t_HV_GIC_ICC_REG_AP1R0_EL1 as u16 + 2,
            hv_gic_icc_reg_t_HV_GIC_ICC_REG_AP1R0_EL1 as u16 + 1,
            hv_gic_icc_reg_t_HV_GIC_ICC_REG_AP1R0_EL1 as u16,
        ];
        for wanted in ordered {
            Self::restore_gic_icc_exact(vcpuid, regs, wanted);
        }
        for &(reg, val) in regs {
            if !ordered.contains(&reg) {
                Self::restore_gic_icc_reg(vcpuid, reg, val);
            }
        }
    }

    fn raw_set_sys_reg(vcpuid: hv_vcpu_t, reg: u16, value: u64) -> Result<(), Error> {
        let ret = unsafe { hv_vcpu_set_sys_reg(vcpuid, reg as hv_sys_reg_t, value) };
        if ret != HV_SUCCESS {
            Err(Error::VcpuSetSystemRegister(reg, value))
        } else {
            Ok(())
        }
    }

    fn raw_get_fp(vcpuid: hv_vcpu_t, reg: hv_simd_fp_reg_t) -> Result<u128, Error> {
        let mut val: hv_simd_fp_uchar16_t = 0;
        let ret = unsafe { hv_vcpu_get_simd_fp_reg(vcpuid, reg, &mut val) };
        if ret != HV_SUCCESS {
            Err(Error::VcpuReadRegister)
        } else {
            Ok(val)
        }
    }

    fn raw_set_fp(vcpuid: hv_vcpu_t, reg: hv_simd_fp_reg_t, value: u128) -> Result<(), Error> {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        let ret = {
            let bytes = value.to_le_bytes();
            unsafe { krun_hv_vcpu_set_simd_fp_reg_from_bytes(vcpuid, reg, bytes.as_ptr()) }
        };
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        let ret = unsafe { hv_vcpu_set_simd_fp_reg(vcpuid, reg, value) };
        if ret != HV_SUCCESS {
            Err(Error::VcpuSetRegister)
        } else {
            Ok(())
        }
    }

    pub fn save_state(&self) -> Result<HvfVcpuState, Error> {
        let id = self.vcpuid;

        let mut gp = [0u64; 31];
        for (i, slot) in gp.iter_mut().enumerate() {
            *slot = Self::raw_get_reg(id, hv_reg_t_HV_REG_X0 + i as hv_reg_t)?;
        }
        let pc = Self::raw_get_reg(id, hv_reg_t_HV_REG_PC)?;
        let cpsr = Self::raw_get_reg(id, hv_reg_t_HV_REG_CPSR)?;
        let fpcr = Self::raw_get_reg(id, hv_reg_t_HV_REG_FPCR)?;
        let fpsr = Self::raw_get_reg(id, hv_reg_t_HV_REG_FPSR)?;

        let mut fp = [0u128; 32];
        for (i, slot) in fp.iter_mut().enumerate() {
            *slot = Self::raw_get_fp(id, i as hv_simd_fp_reg_t)?;
        }

        let mut sysregs = Vec::with_capacity(SYS_REGS_EL1.len() + SYS_REGS_EL2.len());
        for &r in SYS_REGS_EL1 {
            match Self::raw_get_sys_reg(id, r) {
                Ok(v) => sysregs.push((r, v)),
                Err(e) => debug!("snapshot save: skipping sysreg 0x{r:x}: {e}"),
            }
        }
        if self.nested_enabled {
            for &r in SYS_REGS_EL2 {
                match Self::raw_get_sys_reg(id, r) {
                    Ok(v) => sysregs.push((r, v)),
                    Err(e) => debug!("snapshot save: skipping EL2 sysreg 0x{r:x}: {e}"),
                }
            }
        }
        let gic_icc_regs = GIC_ICC_REGS
            .iter()
            .filter_map(|&r| Self::raw_get_gic_icc_reg(id, r).map(|v| (r, v)))
            .collect::<Vec<_>>();
        let gic_redist_regs = GIC_REDIST_REGS
            .iter()
            .filter_map(|&r| Self::raw_get_gic_redist_reg(id, r).map(|v| (r, v)))
            .collect::<Vec<_>>();
        let gic_ich_regs = if self.nested_enabled {
            GIC_ICH_REGS
                .iter()
                .filter_map(|&r| Self::raw_get_gic_ich_reg(id, r).map(|v| (r, v)))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        let mut vtimer_offset: u64 = 0;
        // SAFETY: FFI call.
        let _ = unsafe { hv_vcpu_get_vtimer_offset(id, &mut vtimer_offset as *mut _) };
        Ok(HvfVcpuState {
            gp,
            pc,
            cpsr,
            fpcr,
            fpsr,
            fp,
            sysregs,
            gic_icc_regs,
            gic_redist_regs,
            gic_ich_regs,
            vtimer_masked: self.vtimer_masked,
            vtimer_offset,
        })
    }

    pub fn restore_state(&mut self, st: &HvfVcpuState) -> Result<(), Error> {
        let id = self.vcpuid;

        for (i, &v) in st.gp.iter().enumerate() {
            Self::raw_set_reg(id, hv_reg_t_HV_REG_X0 + i as hv_reg_t, v)?;
        }
        Self::raw_set_reg(id, hv_reg_t_HV_REG_PC, st.pc)?;

        for &(reg, val) in &st.sysregs {
            if is_el2_control_reg(reg) {
                restore_sys_reg(id, reg, val);
            }
        }
        for &(reg, val) in &st.sysregs {
            if !is_el2_control_reg(reg) {
                restore_sys_reg(id, reg, val);
            }
        }

        Self::raw_set_reg(id, hv_reg_t_HV_REG_CPSR, st.cpsr)?;
        Self::raw_set_reg(id, hv_reg_t_HV_REG_FPCR, st.fpcr)?;
        Self::raw_set_reg(id, hv_reg_t_HV_REG_FPSR, st.fpsr)?;

        for (i, &v) in st.fp.iter().enumerate() {
            Self::raw_set_fp(id, i as hv_simd_fp_reg_t, v)?;
        }

        self.restore_gic_redist_regs(&st.gic_redist_regs)?;
        Self::restore_gic_icc_regs(id, &st.gic_icc_regs);
        for &(reg, val) in &st.gic_ich_regs {
            if !Self::raw_set_gic_ich_reg(id, reg, val) {
                debug!("snapshot restore: skipping GIC ICH reg 0x{reg:x}");
            }
        }

        // Restore the captured offset as the baseline. The snapshot
        // orchestrator later re-arms pending timer state before resuming.
        unsafe {
            let _ = hv_vcpu_set_vtimer_offset(id, st.vtimer_offset);
        }
        if let (Ok(cval), Ok(ctl)) = (
            Self::raw_get_sys_reg(id, hv_sys_reg_t_HV_SYS_REG_CNTV_CVAL_EL0 as u16),
            Self::raw_get_sys_reg(id, hv_sys_reg_t_HV_SYS_REG_CNTV_CTL_EL0 as u16),
        ) {
            let _ = Self::raw_set_sys_reg(id, hv_sys_reg_t_HV_SYS_REG_CNTV_CVAL_EL0 as u16, cval);
            let ctl = if (ctl & TMR_CTL_ENABLE) == 0 {
                ctl & !TMR_CTL_ISTATUS
            } else {
                ctl
            };
            let _ = Self::raw_set_sys_reg(id, hv_sys_reg_t_HV_SYS_REG_CNTV_CTL_EL0 as u16, ctl);
        }

        let _ = vcpu_set_vtimer_mask(id, false);
        self.vtimer_masked = false;

        Ok(())
    }

    pub fn restore_gic_redist_regs(&mut self, regs: &[(u32, u64)]) -> Result<(), Error> {
        let id = self.vcpuid;
        Self::restore_gic_redist_exact(
            id,
            regs,
            hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_IGROUPR0 as u32,
        );
        Self::restore_gic_redist_exact(
            id,
            regs,
            hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_ISENABLER0 as u32,
        );
        // Configuration must be restored before pending bits so level/edge
        // state is interpreted the same way as it was at capture.
        Self::restore_gic_redist_exact(
            id,
            regs,
            hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_ICFGR0 as u32,
        );
        Self::restore_gic_redist_exact(
            id,
            regs,
            hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_ICFGR1 as u32,
        );
        Self::restore_gic_redist_exact(
            id,
            regs,
            hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_ISPENDR0 as u32,
        );
        Self::restore_gic_redist_exact(
            id,
            regs,
            hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_ISACTIVER0 as u32,
        );
        Self::restore_gic_redist_range(
            id,
            regs,
            hv_gic_redistributor_reg_t_HV_GIC_REDISTRIBUTOR_REG_GICR_IPRIORITYR0 as u32,
            32,
        );
        for &(reg, val) in regs {
            if !Self::is_ordered_gic_redist_reg(reg) {
                Self::restore_gic_redist_reg(id, reg, val);
            }
        }
        Ok(())
    }

    /// Restore-side timer rebase after host time elapsed while the VM was not
    /// running. HVF exposes CNTVCT_EL0 to the guest as host counter minus the
    /// virtual timer offset, so add the elapsed host ticks to the saved offset
    /// to keep guest virtual time continuous across snapshot downtime.
    pub fn rebase_timer(&self, delta_ticks: u64) -> Result<(), Error> {
        let mut offset: u64 = 0;
        unsafe {
            let _ = hv_vcpu_get_vtimer_offset(self.vcpuid, &mut offset as *mut _);
            let _ = hv_vcpu_set_vtimer_offset(self.vcpuid, offset.wrapping_add(delta_ticks));
        }
        if let (Ok(cval), Ok(ctl)) = (
            Self::raw_get_sys_reg(self.vcpuid, hv_sys_reg_t_HV_SYS_REG_CNTV_CVAL_EL0 as u16),
            Self::raw_get_sys_reg(self.vcpuid, hv_sys_reg_t_HV_SYS_REG_CNTV_CTL_EL0 as u16),
        ) {
            let ctl_to_write = if (ctl & TMR_CTL_ENABLE) == 0 {
                ctl & !TMR_CTL_ISTATUS
            } else {
                ctl
            };
            let _ = vcpu_set_vtimer_mask(self.vcpuid, true);
            let _ = Self::raw_set_sys_reg(
                self.vcpuid,
                hv_sys_reg_t_HV_SYS_REG_CNTV_CVAL_EL0 as u16,
                cval,
            );
            let _ = Self::raw_set_sys_reg(
                self.vcpuid,
                hv_sys_reg_t_HV_SYS_REG_CNTV_CTL_EL0 as u16,
                ctl_to_write,
            );
            let _ = vcpu_set_vtimer_mask(self.vcpuid, false);
        }
        if let Ok(cval) =
            Self::raw_get_sys_reg(self.vcpuid, hv_sys_reg_t_HV_SYS_REG_CNTHP_CVAL_EL2 as u16)
        {
            let _ = Self::raw_set_sys_reg(
                self.vcpuid,
                hv_sys_reg_t_HV_SYS_REG_CNTHP_CVAL_EL2 as u16,
                cval,
            );
            if let Ok(ctl) =
                Self::raw_get_sys_reg(self.vcpuid, hv_sys_reg_t_HV_SYS_REG_CNTHP_CTL_EL2 as u16)
            {
                let ctl = if (ctl & TMR_CTL_ENABLE) == 0 {
                    ctl & !TMR_CTL_ISTATUS
                } else {
                    ctl
                };
                let _ = Self::raw_set_sys_reg(
                    self.vcpuid,
                    hv_sys_reg_t_HV_SYS_REG_CNTHP_CTL_EL2 as u16,
                    ctl,
                );
            }
        }
        Ok(())
    }
}
