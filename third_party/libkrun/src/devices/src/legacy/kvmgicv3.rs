// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io;

use crate::Error as DeviceError;
use crate::bus::BusDevice;
use crate::legacy::gic::GICDevice;
use crate::legacy::irqchip::IrqChipT;

use kvm_ioctls::{DeviceFd, Error, VmFd};
use serde::{Deserialize, Serialize};
use utils::eventfd::EventFd;

const KVM_VGIC_V3_BASE_SIZE: u64 = 0x0001_0000;

// Device trees specific constants
const ARCH_GIC_V3_MAINT_IRQ: u32 = 9;

pub struct KvmGicV3 {
    device_fd: DeviceFd,

    /// GIC device properties, to be used for setting up the fdt entry
    properties: [u64; 4],

    /// Number of CPUs handled by the device
    vcpu_count: u64,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Serialize, Deserialize)]
struct KvmGicV3Snapshot {
    vcpu_count: u64,
    regs32: Vec<DeviceReg32>,
    regs64: Vec<DeviceReg64>,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Serialize, Deserialize)]
struct DeviceReg32 {
    group: u32,
    attr: u64,
    value: u32,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Serialize, Deserialize)]
struct DeviceReg64 {
    group: u32,
    attr: u64,
    value: u64,
}

#[cfg(target_os = "linux")]
const GIC_INTERNAL: u32 = 32;
#[cfg(target_os = "linux")]
const GICD_CTLR: u32 = 0x0000;
#[cfg(target_os = "linux")]
const GICD_STATUSR: u32 = 0x0010;
#[cfg(target_os = "linux")]
const GICD_IGROUPR: u32 = 0x0080;
#[cfg(target_os = "linux")]
const GICD_ISENABLER: u32 = 0x0100;
#[cfg(target_os = "linux")]
const GICD_ICENABLER: u32 = 0x0180;
#[cfg(target_os = "linux")]
const GICD_ISPENDR: u32 = 0x0200;
#[cfg(target_os = "linux")]
const GICD_ISACTIVER: u32 = 0x0300;
#[cfg(target_os = "linux")]
const GICD_ICACTIVER: u32 = 0x0380;
#[cfg(target_os = "linux")]
const GICD_IPRIORITYR: u32 = 0x0400;
#[cfg(target_os = "linux")]
const GICD_ICFGR: u32 = 0x0c00;
#[cfg(target_os = "linux")]
const GICD_IROUTER: u32 = 0x6000;
// KVM's redistributor uaccess space covers both 64KiB frames: the RD_base
// frame at offset 0 and the SGI_base frame (SGI/PPI registers) at 0x10000.
// Frame-relative offsets in the SGI frame read back as zero through the
// device-attr API, silently dropping all PPI/SGI state.
#[cfg(target_os = "linux")]
const GICR_SGI_BASE: u32 = 0x1_0000;
#[cfg(target_os = "linux")]
const GICR_CTLR: u32 = 0x0000;
#[cfg(target_os = "linux")]
const GICR_STATUSR: u32 = 0x0010;
#[cfg(target_os = "linux")]
const GICR_WAKER: u32 = 0x0014;
#[cfg(target_os = "linux")]
const GICR_IGROUPR0: u32 = GICR_SGI_BASE + 0x0080;
#[cfg(target_os = "linux")]
const GICR_ISENABLER0: u32 = GICR_SGI_BASE + 0x0100;
#[cfg(target_os = "linux")]
const GICR_ICENABLER0: u32 = GICR_SGI_BASE + 0x0180;
#[cfg(target_os = "linux")]
const GICR_ISPENDR0: u32 = GICR_SGI_BASE + 0x0200;
#[cfg(target_os = "linux")]
const GICR_ISACTIVER0: u32 = GICR_SGI_BASE + 0x0300;
#[cfg(target_os = "linux")]
const GICR_ICACTIVER0: u32 = GICR_SGI_BASE + 0x0380;
#[cfg(target_os = "linux")]
const GICR_IPRIORITYR: u32 = GICR_SGI_BASE + 0x0400;
#[cfg(target_os = "linux")]
const GICR_ICFGR1: u32 = GICR_SGI_BASE + 0x0c04;
#[cfg(target_os = "linux")]
const ICC_PMR_EL1: u64 = vgic_sysreg(3, 0, 4, 6, 0);
#[cfg(target_os = "linux")]
const ICC_BPR0_EL1: u64 = vgic_sysreg(3, 0, 12, 8, 3);
#[cfg(target_os = "linux")]
const ICC_AP0R_EL1: [u64; 4] = [
    vgic_sysreg(3, 0, 12, 8, 4),
    vgic_sysreg(3, 0, 12, 8, 5),
    vgic_sysreg(3, 0, 12, 8, 6),
    vgic_sysreg(3, 0, 12, 8, 7),
];
#[cfg(target_os = "linux")]
const ICC_AP1R_EL1: [u64; 4] = [
    vgic_sysreg(3, 0, 12, 9, 0),
    vgic_sysreg(3, 0, 12, 9, 1),
    vgic_sysreg(3, 0, 12, 9, 2),
    vgic_sysreg(3, 0, 12, 9, 3),
];
#[cfg(target_os = "linux")]
const ICC_BPR1_EL1: u64 = vgic_sysreg(3, 0, 12, 12, 3);
#[cfg(target_os = "linux")]
const ICC_CTLR_EL1: u64 = vgic_sysreg(3, 0, 12, 12, 4);
#[cfg(target_os = "linux")]
const ICC_SRE_EL1: u64 = vgic_sysreg(3, 0, 12, 12, 5);
#[cfg(target_os = "linux")]
const ICC_IGRPEN0_EL1: u64 = vgic_sysreg(3, 0, 12, 12, 6);
#[cfg(target_os = "linux")]
const ICC_IGRPEN1_EL1: u64 = vgic_sysreg(3, 0, 12, 12, 7);
#[cfg(target_os = "linux")]
const ICC_BASE_SYSREGS: &[u64] = &[
    ICC_PMR_EL1,
    ICC_BPR0_EL1,
    ICC_BPR1_EL1,
    ICC_CTLR_EL1,
    ICC_SRE_EL1,
    ICC_IGRPEN0_EL1,
    ICC_IGRPEN1_EL1,
];

#[cfg(target_os = "linux")]
const fn vgic_sysreg(op0: u64, op1: u64, crn: u64, crm: u64, op2: u64) -> u64 {
    (op0 << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2
}

#[cfg(target_os = "linux")]
fn icc_apr_count(icc_ctlr_el1: u64) -> usize {
    match ((icc_ctlr_el1 >> 8) & 0x7) + 1 {
        7 => 4,
        6 => 2,
        _ => 1,
    }
}

impl KvmGicV3 {
    pub fn new(vm: &VmFd, vcpu_count: u64) -> Result<Self, Error> {
        let dist_size = KVM_VGIC_V3_BASE_SIZE;
        let dist_addr = arch::MMIO_MEM_START - dist_size;
        let redist_size = 2 * dist_size;
        let redists_size = redist_size * vcpu_count;
        let redists_addr = dist_addr - redists_size;

        let mut gic_device = kvm_bindings::kvm_create_device {
            type_: kvm_bindings::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3,
            fd: 0,
            flags: 0,
        };
        let device_fd = vm.create_device(&mut gic_device)?;

        let attr = kvm_bindings::kvm_device_attr {
            group: kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ADDR,
            attr: u64::from(kvm_bindings::KVM_VGIC_V3_ADDR_TYPE_DIST),
            addr: &dist_addr as *const u64 as u64,
            flags: 0,
        };
        device_fd.set_device_attr(&attr)?;

        let attr = kvm_bindings::kvm_device_attr {
            group: kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ADDR,
            attr: u64::from(kvm_bindings::KVM_VGIC_V3_ADDR_TYPE_REDIST),
            addr: &redists_addr as *const u64 as u64,
            flags: 0,
        };
        device_fd.set_device_attr(&attr)?;

        let nr_irqs: u32 = arch::aarch64::layout::IRQ_MAX - arch::aarch64::layout::IRQ_BASE + 1;
        let nr_irqs_ptr = &nr_irqs as *const u32;
        let attr = kvm_bindings::kvm_device_attr {
            group: kvm_bindings::KVM_DEV_ARM_VGIC_GRP_NR_IRQS,
            attr: 0,
            addr: nr_irqs_ptr as u64,
            flags: 0,
        };
        device_fd.set_device_attr(&attr)?;

        let attr = kvm_bindings::kvm_device_attr {
            group: kvm_bindings::KVM_DEV_ARM_VGIC_GRP_CTRL,
            attr: u64::from(kvm_bindings::KVM_DEV_ARM_VGIC_CTRL_INIT),
            addr: 0,
            flags: 0,
        };
        device_fd.set_device_attr(&attr)?;

        Ok(Self {
            device_fd,
            properties: [dist_addr, dist_size, redists_addr, redists_size],
            vcpu_count,
        })
    }

    #[cfg(target_os = "linux")]
    fn attr(offset: u64, mpidr: u64) -> u64 {
        (mpidr << kvm_bindings::KVM_DEV_ARM_VGIC_V3_MPIDR_SHIFT) | offset
    }

    #[cfg(target_os = "linux")]
    fn offset(attr: u64) -> u64 {
        attr & u64::from(kvm_bindings::KVM_DEV_ARM_VGIC_OFFSET_MASK)
    }

    #[cfg(target_os = "linux")]
    fn offset_in_range(offset: u64, base: u32, len: u32) -> bool {
        offset >= u64::from(base) && offset < u64::from(base + len)
    }

    #[cfg(target_os = "linux")]
    fn attr_base(attr: u64) -> u64 {
        attr & !u64::from(kvm_bindings::KVM_DEV_ARM_VGIC_OFFSET_MASK)
    }

    #[cfg(target_os = "linux")]
    fn reg32_is(reg: &DeviceReg32, group: u32, offset: u32) -> bool {
        reg.group == group && Self::offset(reg.attr) == u64::from(offset)
    }

    #[cfg(target_os = "linux")]
    fn write_reg32_clear(
        &self,
        reg: &DeviceReg32,
        clear_offset: u32,
        value: u32,
    ) -> Result<(), DeviceError> {
        self.write_reg32(&DeviceReg32 {
            group: reg.group,
            attr: Self::attr_base(reg.attr) | u64::from(clear_offset),
            value,
        })
    }

    #[cfg(target_os = "linux")]
    fn restore_reg32_where<F>(
        &self,
        regs: &[DeviceReg32],
        mut predicate: F,
    ) -> Result<(), DeviceError>
    where
        F: FnMut(&DeviceReg32) -> bool,
    {
        for reg in regs {
            if predicate(reg) {
                self.write_reg32(reg)?;
            }
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn restore_reg64_where<F>(
        &self,
        regs: &[DeviceReg64],
        mut predicate: F,
    ) -> Result<(), DeviceError>
    where
        F: FnMut(&DeviceReg64) -> bool,
    {
        for reg in regs {
            if predicate(reg) {
                self.write_reg64(reg)?;
            }
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn read_reg32(&self, group: u32, attr: u64) -> Result<DeviceReg32, DeviceError> {
        let mut value = 0u32;
        let mut kattr = kvm_bindings::kvm_device_attr {
            group,
            attr,
            addr: &mut value as *mut u32 as u64,
            flags: 0,
        };
        unsafe { self.device_fd.get_device_attr(&mut kattr) }.map_err(|e| {
            DeviceError::FailedSignalingUsedQueue(io::Error::other(format!(
                "get vgic32 group={} attr=0x{:x}: {e}",
                group, attr
            )))
        })?;
        Ok(DeviceReg32 { group, attr, value })
    }

    #[cfg(target_os = "linux")]
    fn write_reg32(&self, reg: &DeviceReg32) -> Result<(), DeviceError> {
        let mut value = reg.value;
        let kattr = kvm_bindings::kvm_device_attr {
            group: reg.group,
            attr: reg.attr,
            addr: &mut value as *mut u32 as u64,
            flags: 0,
        };
        self.device_fd.set_device_attr(&kattr).map_err(|e| {
            DeviceError::FailedSignalingUsedQueue(io::Error::other(format!(
                "set vgic32 group={} attr=0x{:x}: {e}",
                reg.group, reg.attr
            )))
        })
    }

    #[cfg(target_os = "linux")]
    fn read_reg64(&self, group: u32, attr: u64) -> Result<DeviceReg64, DeviceError> {
        let mut value = 0u64;
        let mut kattr = kvm_bindings::kvm_device_attr {
            group,
            attr,
            addr: &mut value as *mut u64 as u64,
            flags: 0,
        };
        unsafe { self.device_fd.get_device_attr(&mut kattr) }.map_err(|e| {
            DeviceError::FailedSignalingUsedQueue(io::Error::other(format!(
                "get vgic64 group={} attr=0x{:x}: {e}",
                group, attr
            )))
        })?;
        Ok(DeviceReg64 { group, attr, value })
    }

    #[cfg(target_os = "linux")]
    fn write_reg64(&self, reg: &DeviceReg64) -> Result<(), DeviceError> {
        let mut value = reg.value;
        let kattr = kvm_bindings::kvm_device_attr {
            group: reg.group,
            attr: reg.attr,
            addr: &mut value as *mut u64 as u64,
            flags: 0,
        };
        self.device_fd.set_device_attr(&kattr).map_err(|e| {
            DeviceError::FailedSignalingUsedQueue(io::Error::other(format!(
                "set vgic64 group={} attr=0x{:x}: {e}",
                reg.group, reg.attr
            )))
        })
    }

    #[cfg(target_os = "linux")]
    fn save_snapshot(&self) -> Result<Vec<u8>, DeviceError> {
        let mut regs32 = Vec::new();
        let mut regs64 = Vec::new();
        let nr_irqs = arch::aarch64::layout::IRQ_MAX - arch::aarch64::layout::IRQ_BASE + 1;

        for offset in [GICD_CTLR, GICD_STATUSR] {
            regs32.push(self.read_reg32(
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_DIST_REGS,
                Self::attr(offset.into(), 0),
            )?);
        }
        for base in [GICD_IGROUPR, GICD_ISENABLER, GICD_ISPENDR, GICD_ISACTIVER] {
            for irq in (GIC_INTERNAL..nr_irqs).step_by(32) {
                regs32.push(self.read_reg32(
                    kvm_bindings::KVM_DEV_ARM_VGIC_GRP_DIST_REGS,
                    Self::attr((base + irq / 8).into(), 0),
                )?);
            }
        }
        for irq in (GIC_INTERNAL..nr_irqs).step_by(16) {
            regs32.push(self.read_reg32(
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_DIST_REGS,
                Self::attr((GICD_ICFGR + irq / 4).into(), 0),
            )?);
        }
        for irq in (GIC_INTERNAL..nr_irqs).step_by(4) {
            regs32.push(self.read_reg32(
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_DIST_REGS,
                Self::attr((GICD_IPRIORITYR + irq).into(), 0),
            )?);
        }
        for irq in GIC_INTERNAL..nr_irqs {
            let router = GICD_IROUTER + irq * 8;
            for offset in [router, router + 4] {
                regs32.push(self.read_reg32(
                    kvm_bindings::KVM_DEV_ARM_VGIC_GRP_DIST_REGS,
                    Self::attr(offset.into(), 0),
                )?);
            }
        }
        for irq in (GIC_INTERNAL..nr_irqs).step_by(32) {
            regs32.push(self.read_reg32(
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO,
                Self::attr(irq.into(), 0)
                    | ((kvm_bindings::VGIC_LEVEL_INFO_LINE_LEVEL as u64)
                        << kvm_bindings::KVM_DEV_ARM_VGIC_LINE_LEVEL_INFO_SHIFT),
            )?);
        }

        for cpu in 0..self.vcpu_count {
            for offset in [
                GICR_CTLR,
                GICR_STATUSR,
                GICR_WAKER,
                GICR_IGROUPR0,
                GICR_ISENABLER0,
                GICR_ICFGR1,
                GICR_ISPENDR0,
                GICR_ISACTIVER0,
            ] {
                regs32.push(self.read_reg32(
                    kvm_bindings::KVM_DEV_ARM_VGIC_GRP_REDIST_REGS,
                    Self::attr(offset.into(), cpu),
                )?);
            }
            regs32.push(self.read_reg32(
                kvm_bindings::KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO,
                Self::attr(0, cpu)
                    | ((kvm_bindings::VGIC_LEVEL_INFO_LINE_LEVEL as u64)
                        << kvm_bindings::KVM_DEV_ARM_VGIC_LINE_LEVEL_INFO_SHIFT),
            )?);
            for offset in (0..GIC_INTERNAL).step_by(4) {
                regs32.push(self.read_reg32(
                    kvm_bindings::KVM_DEV_ARM_VGIC_GRP_REDIST_REGS,
                    Self::attr((GICR_IPRIORITYR + offset).into(), cpu),
                )?);
            }
            let mut ctlr = None;
            for reg in ICC_BASE_SYSREGS {
                let state = self.read_reg64(
                    kvm_bindings::KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS,
                    Self::attr(*reg, cpu),
                )?;
                if *reg == ICC_CTLR_EL1 {
                    ctlr = Some(state.value);
                }
                regs64.push(state);
            }
            let ap_count = icc_apr_count(ctlr.unwrap_or(0));
            for reg in ICC_AP0R_EL1
                .iter()
                .take(ap_count)
                .chain(ICC_AP1R_EL1.iter().take(ap_count))
            {
                regs64.push(self.read_reg64(
                    kvm_bindings::KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS,
                    Self::attr(*reg, cpu),
                )?);
            }
        }

        bincode::serialize(&KvmGicV3Snapshot {
            vcpu_count: self.vcpu_count,
            regs32,
            regs64,
        })
        .map_err(|e| DeviceError::FailedSignalingUsedQueue(io::Error::other(e)))
    }

    #[cfg(target_os = "linux")]
    fn restore_snapshot(&self, data: &[u8]) -> Result<(), DeviceError> {
        let snapshot: KvmGicV3Snapshot = bincode::deserialize(data)
            .map_err(|e| DeviceError::FailedSignalingUsedQueue(io::Error::other(e)))?;
        if snapshot.vcpu_count != self.vcpu_count {
            return Err(DeviceError::FailedSignalingUsedQueue(io::Error::other(
                "snapshot vCPU count does not match KVM GIC",
            )));
        }

        let redist = kvm_bindings::KVM_DEV_ARM_VGIC_GRP_REDIST_REGS;
        let dist = kvm_bindings::KVM_DEV_ARM_VGIC_GRP_DIST_REGS;
        let level = kvm_bindings::KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO;
        let cpu_sysregs = kvm_bindings::KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS;

        self.restore_reg32_where(&snapshot.regs32, |reg| Self::reg32_is(reg, dist, GICD_CTLR))?;

        self.restore_reg32_where(&snapshot.regs32, |reg| {
            reg.group == redist
                && matches!(
                    Self::offset(reg.attr) as u32,
                    GICR_CTLR | GICR_STATUSR | GICR_WAKER | GICR_IGROUPR0
                )
        })?;
        for reg in &snapshot.regs32 {
            if Self::reg32_is(reg, redist, GICR_ISENABLER0) {
                self.write_reg32_clear(reg, GICR_ICENABLER0, u32::MAX)?;
                self.write_reg32(reg)?;
            }
        }
        self.restore_reg32_where(&snapshot.regs32, |reg| {
            Self::reg32_is(reg, redist, GICR_ICFGR1)
                || (reg.group == level && Self::offset(reg.attr) < u64::from(GIC_INTERNAL))
        })?;
        self.restore_reg32_where(&snapshot.regs32, |reg| {
            Self::reg32_is(reg, redist, GICR_ISPENDR0)
        })?;
        for reg in &snapshot.regs32 {
            if Self::reg32_is(reg, redist, GICR_ISACTIVER0) {
                self.write_reg32_clear(reg, GICR_ICACTIVER0, u32::MAX)?;
                self.write_reg32(reg)?;
            }
        }
        self.restore_reg32_where(&snapshot.regs32, |reg| {
            reg.group == redist
                && Self::offset_in_range(Self::offset(reg.attr), GICR_IPRIORITYR, GIC_INTERNAL)
        })?;

        self.restore_reg32_where(&snapshot.regs32, |reg| {
            Self::reg32_is(reg, dist, GICD_STATUSR)
        })?;
        for reg in &snapshot.regs32 {
            let offset = Self::offset(reg.attr);
            if reg.group == dist && Self::offset_in_range(offset, GICD_ISENABLER, 0x80) {
                self.write_reg32_clear(
                    reg,
                    GICD_ICENABLER + (offset as u32 - GICD_ISENABLER),
                    u32::MAX,
                )?;
                self.write_reg32(reg)?;
            }
        }
        self.restore_reg32_where(&snapshot.regs32, |reg| {
            reg.group == dist && Self::offset_in_range(Self::offset(reg.attr), GICD_IGROUPR, 0x80)
        })?;
        self.restore_reg32_where(&snapshot.regs32, |reg| {
            reg.group == dist && Self::offset_in_range(Self::offset(reg.attr), GICD_IROUTER, 0x2000)
        })?;
        self.restore_reg32_where(&snapshot.regs32, |reg| {
            reg.group == dist && Self::offset_in_range(Self::offset(reg.attr), GICD_ICFGR, 0x100)
        })?;
        self.restore_reg32_where(&snapshot.regs32, |reg| reg.group == level)?;
        self.restore_reg32_where(&snapshot.regs32, |reg| {
            reg.group == dist && Self::offset_in_range(Self::offset(reg.attr), GICD_ISPENDR, 0x80)
        })?;
        for reg in &snapshot.regs32 {
            let offset = Self::offset(reg.attr);
            if reg.group == dist && Self::offset_in_range(offset, GICD_ISACTIVER, 0x80) {
                self.write_reg32_clear(
                    reg,
                    GICD_ICACTIVER + (offset as u32 - GICD_ISACTIVER),
                    u32::MAX,
                )?;
                self.write_reg32(reg)?;
            }
        }
        self.restore_reg32_where(&snapshot.regs32, |reg| {
            reg.group == dist
                && Self::offset_in_range(Self::offset(reg.attr), GICD_IPRIORITYR, 0x400)
        })?;

        for sysreg in [
            ICC_SRE_EL1,
            ICC_CTLR_EL1,
            ICC_IGRPEN0_EL1,
            ICC_IGRPEN1_EL1,
            ICC_PMR_EL1,
            ICC_BPR0_EL1,
            ICC_BPR1_EL1,
        ] {
            self.restore_reg64_where(&snapshot.regs64, |reg| {
                reg.group == cpu_sysregs && Self::offset(reg.attr) == sysreg
            })?;
        }
        for aprs in [ICC_AP0R_EL1, ICC_AP1R_EL1] {
            for sysreg in aprs.iter().rev() {
                self.restore_reg64_where(&snapshot.regs64, |reg| {
                    reg.group == cpu_sysregs && Self::offset(reg.attr) == *sysreg
                })?;
            }
        }
        Ok(())
    }
}

impl IrqChipT for KvmGicV3 {
    fn get_mmio_addr(&self) -> u64 {
        0
    }

    fn get_mmio_size(&self) -> u64 {
        0
    }

    fn set_irq(
        &self,
        _irq_line: Option<u32>,
        interrupt_evt: Option<&EventFd>,
    ) -> Result<(), DeviceError> {
        if let Some(interrupt_evt) = interrupt_evt {
            if let Err(e) = interrupt_evt.write(1) {
                error!("Failed to signal used queue: {e:?}");
                return Err(DeviceError::FailedSignalingUsedQueue(e));
            }
        } else {
            error!("EventFd not set up for irq line");
            return Err(DeviceError::FailedSignalingUsedQueue(io::Error::new(
                io::ErrorKind::NotFound,
                "EventFd not set up for irq line".to_string(),
            )));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn snapshot_state(&self) -> Result<Option<Vec<u8>>, DeviceError> {
        self.save_snapshot().map(Some)
    }

    #[cfg(target_os = "linux")]
    fn restore_snapshot_state(&mut self, data: &[u8]) -> Result<(), DeviceError> {
        self.restore_snapshot(data)
    }
}

impl BusDevice for KvmGicV3 {
    fn read(&mut self, _vcpuid: u64, _offset: u64, _data: &mut [u8]) {
        unreachable!("MMIO operations are managed in-kernel");
    }

    fn write(&mut self, _vcpuid: u64, _offset: u64, _data: &[u8]) {
        unreachable!("MMIO operations are managed in-kernel");
    }
}

impl GICDevice for KvmGicV3 {
    fn device_properties(&self) -> Vec<u64> {
        self.properties.to_vec()
    }

    fn vcpu_count(&self) -> u64 {
        self.vcpu_count
    }

    fn fdt_compatibility(&self) -> String {
        "arm,gic-v3".to_string()
    }

    fn fdt_maint_irq(&self) -> u32 {
        ARCH_GIC_V3_MAINT_IRQ
    }

    fn version(&self) -> u32 {
        kvm_bindings::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3
    }
}
