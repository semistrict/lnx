// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::ffi::c_void;
use std::io;
use std::sync::{Arc, LazyLock};

use crate::Error as DeviceError;
use crate::bus::BusDevice;
use crate::legacy::VcpuList;
use crate::legacy::gic::GICDevice;
use crate::legacy::irqchip::{HvfGicDistReg, IrqChipT, LinuxGicDistReg, LinuxGicDistRestorePhase};

use hvf::Error;
use hvf::bindings::{
    HV_SUCCESS, hv_gic_config_t, hv_gic_distributor_reg_t, hv_gic_state_t, hv_ipa_t, hv_return_t,
};
use utils::eventfd::EventFd;

// Device trees specific constants
const ARCH_GIC_V3_MAINT_IRQ: u32 = 9;
const KVM_DEV_ARM_VGIC_GRP_DIST_REGS: u32 = 1;
const KVM_DEV_ARM_VGIC_OFFSET_MASK: u64 = 0xffff_ffff;
const GIC_INTERNAL: u32 = 32;
const GICD_CTLR: u32 = 0x0000;
const GICD_STATUSR: u32 = 0x0010;
const GICD_IGROUPR: u32 = 0x0080;
const GICD_ISENABLER: u32 = 0x0100;
const GICD_ICENABLER: u32 = 0x0180;
const GICD_ISPENDR: u32 = 0x0200;
const GICD_ICPENDR: u32 = 0x0280;
const GICD_ISACTIVER: u32 = 0x0300;
const GICD_ICACTIVER: u32 = 0x0380;
const GICD_IPRIORITYR: u32 = 0x0400;
const GICD_ICFGR: u32 = 0x0c00;
const GICD_IROUTER: u32 = 0x6000;

unsafe extern "C" {
    fn os_release(object: *const c_void);
}

struct HvfGicStateObject(hv_gic_state_t);

impl Drop for HvfGicStateObject {
    fn drop(&mut self) {
        unsafe { os_release(self.0.cast::<c_void>()) };
    }
}

pub struct HvfGicBindings {
    hv_gic_create:
        libloading::Symbol<'static, unsafe extern "C" fn(hv_gic_config_t) -> hv_return_t>,
    hv_gic_config_create: libloading::Symbol<'static, unsafe extern "C" fn() -> hv_gic_config_t>,
    hv_gic_config_set_distributor_base:
        libloading::Symbol<'static, unsafe extern "C" fn(hv_gic_config_t, hv_ipa_t) -> hv_return_t>,
    hv_gic_config_set_redistributor_base:
        libloading::Symbol<'static, unsafe extern "C" fn(hv_gic_config_t, hv_ipa_t) -> hv_return_t>,
    hv_gic_get_distributor_size:
        libloading::Symbol<'static, unsafe extern "C" fn(*mut usize) -> hv_return_t>,
    hv_gic_get_redistributor_size:
        libloading::Symbol<'static, unsafe extern "C" fn(*mut usize) -> hv_return_t>,
    hv_gic_set_spi: libloading::Symbol<'static, unsafe extern "C" fn(u32, bool) -> hv_return_t>,
    hv_gic_state_create: libloading::Symbol<'static, unsafe extern "C" fn() -> hv_gic_state_t>,
    hv_gic_state_get_size: libloading::Symbol<
        'static,
        unsafe extern "C" fn(hv_gic_state_t, *mut usize) -> hv_return_t,
    >,
    hv_gic_state_get_data: libloading::Symbol<
        'static,
        unsafe extern "C" fn(hv_gic_state_t, *mut c_void) -> hv_return_t,
    >,
    hv_gic_set_state:
        libloading::Symbol<'static, unsafe extern "C" fn(*const c_void, usize) -> hv_return_t>,
    hv_gic_get_distributor_reg: libloading::Symbol<
        'static,
        unsafe extern "C" fn(hv_gic_distributor_reg_t, *mut u64) -> hv_return_t,
    >,
    hv_gic_set_distributor_reg: libloading::Symbol<
        'static,
        unsafe extern "C" fn(hv_gic_distributor_reg_t, u64) -> hv_return_t,
    >,
}

pub struct HvfGicV3 {
    bindings: HvfGicBindings,

    /// GIC device properties, to be used for setting up the fdt entry
    properties: [u64; 4],

    /// Number of CPUs handled by the device
    vcpu_count: u64,

    vcpu_list: Arc<VcpuList>,
}

static HVF: LazyLock<libloading::Library> = LazyLock::new(|| unsafe {
    libloading::Library::new(
        "/System/Library/Frameworks/Hypervisor.framework/Versions/A/Hypervisor",
    )
    .unwrap()
});

impl HvfGicV3 {
    pub fn new(vcpu_count: u64, vcpu_list: Arc<VcpuList>) -> Result<Self, Error> {
        let bindings = unsafe {
            HvfGicBindings {
                hv_gic_create: HVF.get(b"hv_gic_create").map_err(Error::FindSymbol)?,
                hv_gic_config_create: HVF
                    .get(b"hv_gic_config_create")
                    .map_err(Error::FindSymbol)?,
                hv_gic_config_set_distributor_base: HVF
                    .get(b"hv_gic_config_set_distributor_base")
                    .map_err(Error::FindSymbol)?,
                hv_gic_config_set_redistributor_base: HVF
                    .get(b"hv_gic_config_set_redistributor_base")
                    .map_err(Error::FindSymbol)?,
                hv_gic_get_distributor_size: HVF
                    .get(b"hv_gic_get_distributor_size")
                    .map_err(Error::FindSymbol)?,
                hv_gic_get_redistributor_size: HVF
                    .get(b"hv_gic_get_redistributor_size")
                    .map_err(Error::FindSymbol)?,
                hv_gic_set_spi: HVF.get(b"hv_gic_set_spi").map_err(Error::FindSymbol)?,
                hv_gic_state_create: HVF.get(b"hv_gic_state_create").map_err(Error::FindSymbol)?,
                hv_gic_state_get_size: HVF
                    .get(b"hv_gic_state_get_size")
                    .map_err(Error::FindSymbol)?,
                hv_gic_state_get_data: HVF
                    .get(b"hv_gic_state_get_data")
                    .map_err(Error::FindSymbol)?,
                hv_gic_set_state: HVF.get(b"hv_gic_set_state").map_err(Error::FindSymbol)?,
                hv_gic_get_distributor_reg: HVF
                    .get(b"hv_gic_get_distributor_reg")
                    .map_err(Error::FindSymbol)?,
                hv_gic_set_distributor_reg: HVF
                    .get(b"hv_gic_set_distributor_reg")
                    .map_err(Error::FindSymbol)?,
            }
        };

        let mut dist_size: usize = 0;
        let ret = unsafe { (bindings.hv_gic_get_distributor_size)(&mut dist_size) };
        if ret != HV_SUCCESS {
            return Err(Error::VmCreate);
        }
        let dist_size = dist_size as u64;

        let mut redist_size: usize = 0;
        let ret = unsafe { (bindings.hv_gic_get_redistributor_size)(&mut redist_size) };
        if ret != HV_SUCCESS {
            return Err(Error::VmCreate);
        }

        let redists_size = redist_size as u64 * vcpu_count;
        let dist_addr = arch::MMIO_MEM_START - dist_size - redists_size;
        let redists_addr = arch::MMIO_MEM_START - redists_size;

        let gic_config = unsafe { (bindings.hv_gic_config_create)() };
        let ret = unsafe { (bindings.hv_gic_config_set_distributor_base)(gic_config, dist_addr) };
        if ret != HV_SUCCESS {
            return Err(Error::VmCreate);
        }

        let ret =
            unsafe { (bindings.hv_gic_config_set_redistributor_base)(gic_config, redists_addr) };
        if ret != HV_SUCCESS {
            return Err(Error::VmCreate);
        }

        let ret = unsafe { (bindings.hv_gic_create)(gic_config) };
        if ret != HV_SUCCESS {
            return Err(Error::VmCreate);
        }

        Ok(Self {
            bindings,
            properties: [dist_addr, dist_size, redists_addr, redists_size],
            vcpu_count,
            vcpu_list,
        })
    }

    fn hvf_snapshot_state(&self) -> Result<Vec<u8>, DeviceError> {
        let state = unsafe { (self.bindings.hv_gic_state_create)() };
        if state.is_null() {
            return Err(DeviceError::FailedSignalingUsedQueue(io::Error::other(
                "HVF returned null GIC state",
            )));
        }
        let state = HvfGicStateObject(state);

        let mut size = 0usize;
        let ret = unsafe { (self.bindings.hv_gic_state_get_size)(state.0, &mut size) };
        if ret != HV_SUCCESS {
            return Err(DeviceError::FailedSignalingUsedQueue(io::Error::other(
                "HVF returned error when sizing GIC state",
            )));
        }

        let mut data = vec![0u8; size];
        let ret = unsafe {
            (self.bindings.hv_gic_state_get_data)(state.0, data.as_mut_ptr().cast::<c_void>())
        };
        if ret != HV_SUCCESS {
            return Err(DeviceError::FailedSignalingUsedQueue(io::Error::other(
                "HVF returned error when reading GIC state",
            )));
        }

        Ok(data)
    }

    fn hvf_snapshot_distributor_state(&self) -> Result<Vec<HvfGicDistReg>, DeviceError> {
        let max_irq = arch::aarch64::layout::IRQ_MAX;
        let mut regs = Vec::new();

        for offset in [GICD_CTLR, GICD_STATUSR] {
            if let Some(value) = self.hvf_get_dist_reg(offset)? {
                regs.push(HvfGicDistReg { offset, value });
            }
        }

        for base in [GICD_IGROUPR, GICD_ISENABLER, GICD_ISPENDR, GICD_ISACTIVER] {
            for irq in (GIC_INTERNAL..=max_irq).step_by(32) {
                let offset = base + irq / 8;
                if let Some(value) = self.hvf_get_dist_reg(offset)? {
                    regs.push(HvfGicDistReg { offset, value });
                }
            }
        }

        for irq in (GIC_INTERNAL..=max_irq).step_by(16) {
            let offset = GICD_ICFGR + irq / 4;
            if let Some(value) = self.hvf_get_dist_reg(offset)? {
                regs.push(HvfGicDistReg { offset, value });
            }
        }

        for irq in (GIC_INTERNAL..=max_irq).step_by(4) {
            let offset = GICD_IPRIORITYR + irq;
            if let Some(value) = self.hvf_get_dist_reg(offset)? {
                regs.push(HvfGicDistReg { offset, value });
            }
        }

        for irq in GIC_INTERNAL..=max_irq {
            let offset = GICD_IROUTER + irq * 8;
            if let Some(value) = self.hvf_get_dist_reg(offset)? {
                regs.push(HvfGicDistReg { offset, value });
            }
        }

        Ok(regs)
    }

    fn hvf_restore_state(&self, data: &[u8]) -> Result<(), DeviceError> {
        let ret =
            unsafe { (self.bindings.hv_gic_set_state)(data.as_ptr().cast::<c_void>(), data.len()) };
        if ret != HV_SUCCESS {
            return Err(DeviceError::FailedSignalingUsedQueue(io::Error::other(
                "HVF returned error when restoring GIC state",
            )));
        }
        Ok(())
    }

    fn hvf_get_dist_reg(&self, offset: u32) -> Result<Option<u64>, DeviceError> {
        let reg = hv_gic_distributor_reg_t::try_from(offset).map_err(|_| {
            DeviceError::FailedSignalingUsedQueue(io::Error::other(format!(
                "HVF GIC distributor offset 0x{offset:x} does not fit HVF register type"
            )))
        })?;
        let mut value = 0u64;
        let ret = unsafe { (self.bindings.hv_gic_get_distributor_reg)(reg, &mut value) };
        if ret != HV_SUCCESS {
            return Ok(None);
        }
        Ok(Some(value))
    }

    fn hvf_set_dist_reg(&self, offset: u32, value: u64) -> Result<(), DeviceError> {
        let reg = hv_gic_distributor_reg_t::try_from(offset).map_err(|_| {
            DeviceError::FailedSignalingUsedQueue(io::Error::other(format!(
                "Linux GIC distributor offset 0x{offset:x} does not fit HVF register type"
            )))
        })?;
        let ret = unsafe { (self.bindings.hv_gic_set_distributor_reg)(reg, value) };
        if ret != HV_SUCCESS {
            return Err(DeviceError::FailedSignalingUsedQueue(io::Error::other(
                format!("HVF returned {ret} when restoring GICD offset=0x{offset:x}"),
            )));
        }
        Ok(())
    }

    fn hvf_restore_linux_dist_state(
        &self,
        regs: &[LinuxGicDistReg],
        phase: LinuxGicDistRestorePhase,
    ) -> Result<(), DeviceError> {
        match phase {
            LinuxGicDistRestorePhase::Ctlr => {
                self.restore_linux_dist_exact(regs, GICD_CTLR)?;
            }
            LinuxGicDistRestorePhase::Shared => {
                self.restore_linux_dist_set_clear(regs, GICD_ISENABLER, GICD_ICENABLER, 0x80)?;
                self.restore_linux_dist_range(regs, GICD_IGROUPR, 0x80)?;
                self.restore_linux_dist_irouter(regs)?;
                self.restore_linux_dist_range(regs, GICD_ICFGR, 0x100)?;
                self.restore_linux_dist_set_clear(regs, GICD_ISPENDR, GICD_ICPENDR, 0x80)?;
                self.restore_linux_dist_set_clear(regs, GICD_ISACTIVER, GICD_ICACTIVER, 0x80)?;
                self.restore_linux_dist_range(regs, GICD_IPRIORITYR, 0x400)?;
            }
        }
        Ok(())
    }

    fn restore_linux_dist_exact(
        &self,
        regs: &[LinuxGicDistReg],
        wanted_offset: u32,
    ) -> Result<(), DeviceError> {
        for reg in regs.iter().filter(|reg| {
            reg.group == KVM_DEV_ARM_VGIC_GRP_DIST_REGS
                && kvm_gic_attr_offset(reg.attr) == wanted_offset
        }) {
            self.hvf_set_dist_reg(wanted_offset, u64::from(reg.value))?;
        }
        Ok(())
    }

    fn restore_linux_dist_range(
        &self,
        regs: &[LinuxGicDistReg],
        base: u32,
        len: u32,
    ) -> Result<(), DeviceError> {
        for reg in regs
            .iter()
            .filter(|reg| reg.group == KVM_DEV_ARM_VGIC_GRP_DIST_REGS)
        {
            let offset = kvm_gic_attr_offset(reg.attr);
            if offset_in_range(offset, base, len) {
                self.hvf_set_dist_reg(offset, u64::from(reg.value))?;
            }
        }
        Ok(())
    }

    fn restore_linux_dist_set_clear(
        &self,
        regs: &[LinuxGicDistReg],
        set_base: u32,
        clear_base: u32,
        len: u32,
    ) -> Result<(), DeviceError> {
        for reg in regs
            .iter()
            .filter(|reg| reg.group == KVM_DEV_ARM_VGIC_GRP_DIST_REGS)
        {
            let offset = kvm_gic_attr_offset(reg.attr);
            if offset_in_range(offset, set_base, len) {
                let clear_offset = clear_base + (offset - set_base);
                self.hvf_set_dist_reg(clear_offset, 0)?;
                self.hvf_set_dist_reg(offset, u64::from(reg.value))?;
            }
        }
        Ok(())
    }

    fn restore_linux_dist_irouter(&self, regs: &[LinuxGicDistReg]) -> Result<(), DeviceError> {
        for reg in regs
            .iter()
            .filter(|reg| reg.group == KVM_DEV_ARM_VGIC_GRP_DIST_REGS)
        {
            let offset = kvm_gic_attr_offset(reg.attr);
            if !offset_in_range(
                offset,
                GICD_IROUTER + GIC_INTERNAL * 8,
                0x2000 - GIC_INTERNAL * 8,
            ) || ((offset - GICD_IROUTER) % 8) != 0
            {
                continue;
            }

            let high = linux_dist_reg_value(regs, offset + 4).unwrap_or(0);
            let value = u64::from(reg.value) | (u64::from(high) << 32);
            self.hvf_set_dist_reg(offset, value)?;
        }
        Ok(())
    }
}

fn kvm_gic_attr_offset(attr: u64) -> u32 {
    (attr & KVM_DEV_ARM_VGIC_OFFSET_MASK) as u32
}

fn offset_in_range(offset: u32, base: u32, len: u32) -> bool {
    offset >= base && offset < base + len
}

fn linux_dist_reg_value(regs: &[LinuxGicDistReg], wanted_offset: u32) -> Option<u32> {
    regs.iter()
        .find(|reg| {
            reg.group == KVM_DEV_ARM_VGIC_GRP_DIST_REGS
                && kvm_gic_attr_offset(reg.attr) == wanted_offset
        })
        .map(|reg| reg.value)
}

impl IrqChipT for HvfGicV3 {
    fn get_mmio_addr(&self) -> u64 {
        0
    }

    fn get_mmio_size(&self) -> u64 {
        0
    }

    fn set_irq(
        &self,
        irq_line: Option<u32>,
        _interrupt_evt: Option<&EventFd>,
    ) -> Result<(), DeviceError> {
        if let Some(irq_line) = irq_line {
            let ret = unsafe { (self.bindings.hv_gic_set_spi)(irq_line, true) };
            if ret != HV_SUCCESS {
                Err(DeviceError::FailedSignalingUsedQueue(
                    std::io::Error::other("HVF returned error when setting SPI"),
                ))
            } else {
                self.vcpu_list.wake_all();
                Ok(())
            }
        } else {
            Err(DeviceError::FailedSignalingUsedQueue(io::Error::new(
                io::ErrorKind::InvalidData,
                "IRQ not line configured",
            )))
        }
    }

    fn clear_irq(&self, irq_line: Option<u32>) -> Result<(), DeviceError> {
        if let Some(irq_line) = irq_line {
            let ret = unsafe { (self.bindings.hv_gic_set_spi)(irq_line, false) };
            if ret != HV_SUCCESS {
                Err(DeviceError::FailedSignalingUsedQueue(
                    std::io::Error::other("HVF returned error when clearing SPI"),
                ))
            } else {
                Ok(())
            }
        } else {
            Err(DeviceError::FailedSignalingUsedQueue(io::Error::new(
                io::ErrorKind::InvalidData,
                "IRQ not line configured",
            )))
        }
    }

    fn snapshot_state(&self) -> Result<Option<Vec<u8>>, DeviceError> {
        self.hvf_snapshot_state().map(Some)
    }

    fn snapshot_distributor_state(&self) -> Result<Option<Vec<HvfGicDistReg>>, DeviceError> {
        self.hvf_snapshot_distributor_state().map(Some)
    }

    fn restore_snapshot_state(&mut self, data: &[u8]) -> Result<(), DeviceError> {
        self.hvf_restore_state(data)
    }

    fn restore_linux_gic_dist_state(
        &mut self,
        regs: &[LinuxGicDistReg],
        phase: LinuxGicDistRestorePhase,
    ) -> Result<(), DeviceError> {
        self.hvf_restore_linux_dist_state(regs, phase)
    }
}

impl BusDevice for HvfGicV3 {
    fn read(&mut self, _vcpuid: u64, _offset: u64, _data: &mut [u8]) {
        unreachable!("MMIO operations are managed in-kernel");
    }

    fn write(&mut self, _vcpuid: u64, _offset: u64, _data: &[u8]) {
        unreachable!("MMIO operations are managed in-kernel");
    }
}

impl GICDevice for HvfGicV3 {
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
        7
    }
}
