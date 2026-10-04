// Snapshot/restore orchestrator for the HVF backend.
//
// Capture flow:
//   1. Send Pause to every vCPU. Force-exit via hv_vcpus_exit so the vCPU
//      thread returns from hv_vcpu_run and processes the event.
//   2. Each vCPU thread serializes its HvfVcpuState and replies Paused(bytes).
//   3. Walk virtio MMIO transports: pause() each underlying device, then
//      serialize_state(). Collect MmioTransportState too.
//   4. Capture GICv3 state (distributor + per-vCPU pending IRQ bitmaps).
//   5. Write or clone-and-patch pages.img from guest memory. Patching is only
//      allowed when <path> holds the image the dirty tracker is relative to.
//   6. Assemble vmstate.bin (META + new pages.img id + per-vcpu + GICDIST +
//      GICVCPU + per-virtio).
//   7. Atomic publish: write into a staging directory, rename to <path>, then
//      re-arm dirty tracking relative to the new image.
//   8. Resume devices, then vCPUs.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::info;

use devices::legacy::{
    GicV3, GicV3State, IrqChip, LinuxGicDistReg, LinuxGicDistRestorePhase, VcpuList, VcpuListState,
    gic::GICDevice,
};
use devices::virtio::{Descriptor, DeviceSnapshot, MmioTransport, MmioTransportState, QueueState};
use serde::{Deserialize, Serialize};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

pub(crate) use crate::snapshot_metadata::MetaSection;
use crate::snapshot_metadata::{
    self, GUEST_ARCH_AARCH64, GicTopology, PAUTH_POLICY_NOPAUTH, SOURCE_BACKEND_HVF,
    SnapshotFormat, TOPOLOGY_HASH_VERSION, VirtioTopology,
};
use crate::vstate::{KvmGicVcpuState, VcpuEvent, VcpuHandle, VcpuResponse, VcpuTicket};

use super::container::{SectionId, SnapshotWriter};
use super::ram::{clone_and_patch_dirty_pages_img, write_full_pages_img};
use super::{Result, SnapshotError};

const VCPU_PAUSE_TIMEOUT_MS: u64 = 2000;

/// Snapshot of a single virtio-mmio device: transport-side state + the
/// per-device payload returned by `VirtioDevice::serialize_state`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VirtioMmioSection {
    pub mmio_base: u64,
    pub device_type: u32,
    pub transport: MmioTransportState,
    /// None when the device doesn't implement per-device snapshot. The
    /// transport state is still recorded so the guest driver sees the
    /// expected MMIO programming on resume.
    pub device: Option<DeviceSnapshot>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LinuxVirtioMmioSection {
    pub mmio_base: u64,
    pub device_type: String,
    pub transport: MmioTransportState,
    pub device: Option<DeviceSnapshot>,
}

#[derive(Clone, Debug, Deserialize)]
struct KvmGicV3SnapshotCompat {
    vcpu_count: u64,
    regs32: Vec<KvmDeviceReg32Compat>,
    regs64: Vec<KvmDeviceReg64Compat>,
}

#[derive(Clone, Debug, Deserialize)]
struct KvmDeviceReg32Compat {
    group: u32,
    attr: u64,
    value: u32,
}

#[derive(Clone, Debug, Deserialize)]
struct KvmDeviceReg64Compat {
    group: u32,
    attr: u64,
    value: u64,
}

#[derive(Clone, Debug, Default)]
struct RestoredLinuxGicState {
    vcpus: Vec<KvmGicVcpuState>,
    dist_regs: Vec<LinuxGicDistReg>,
    pending_spis: Vec<u32>,
}

enum RestoredVirtioMmioSection {
    Macos(VirtioMmioSection),
    Linux(LinuxVirtioMmioSection),
}

const KVM_DEV_ARM_VGIC_GRP_DIST_REGS: u32 = 1;
const KVM_DEV_ARM_VGIC_GRP_REDIST_REGS: u32 = 5;
const KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS: u32 = 6;
const KVM_DEV_ARM_VGIC_V3_MPIDR_SHIFT: u64 = 32;
const KVM_DEV_ARM_VGIC_OFFSET_MASK: u64 = 0xffff_ffff;
const GIC_INTERNAL: u32 = 32;
const ICH_AP0R_EL2: [u64; 4] = [
    kvm_vgic_sysreg(3, 4, 12, 8, 0),
    kvm_vgic_sysreg(3, 4, 12, 8, 1),
    kvm_vgic_sysreg(3, 4, 12, 8, 2),
    kvm_vgic_sysreg(3, 4, 12, 8, 3),
];
const ICH_AP1R_EL2: [u64; 4] = [
    kvm_vgic_sysreg(3, 4, 12, 9, 0),
    kvm_vgic_sysreg(3, 4, 12, 9, 1),
    kvm_vgic_sysreg(3, 4, 12, 9, 2),
    kvm_vgic_sysreg(3, 4, 12, 9, 3),
];
const ICC_AP0R_EL1: [u64; 4] = [
    kvm_vgic_sysreg(3, 0, 12, 8, 4),
    kvm_vgic_sysreg(3, 0, 12, 8, 5),
    kvm_vgic_sysreg(3, 0, 12, 8, 6),
    kvm_vgic_sysreg(3, 0, 12, 8, 7),
];
const ICC_AP1R_EL1: [u64; 4] = [
    kvm_vgic_sysreg(3, 0, 12, 9, 0),
    kvm_vgic_sysreg(3, 0, 12, 9, 1),
    kvm_vgic_sysreg(3, 0, 12, 9, 2),
    kvm_vgic_sysreg(3, 0, 12, 9, 3),
];
const ICH_HCR_EL2: u64 = kvm_vgic_sysreg(3, 4, 12, 11, 0);
const ICH_VMCR_EL2: u64 = kvm_vgic_sysreg(3, 4, 12, 11, 7);
const ICH_LR0_EL2: u64 = kvm_vgic_sysreg(3, 4, 12, 12, 0);
const ICH_LR8_EL2: u64 = kvm_vgic_sysreg(3, 4, 12, 13, 0);
const GICD_ISPENDR: u32 = 0x0200;
const GICR_SGI_BASE: u32 = 0x1_0000;
const GICR_IGROUPR0: u32 = GICR_SGI_BASE + 0x0080;
const GICR_ISENABLER0: u32 = GICR_SGI_BASE + 0x0100;
const GICR_ISPENDR0: u32 = GICR_SGI_BASE + 0x0200;
const GICR_ISACTIVER0: u32 = GICR_SGI_BASE + 0x0300;
const GICR_IPRIORITYR: u32 = GICR_SGI_BASE + 0x0400;
const GICR_ICFGR0: u32 = GICR_SGI_BASE + 0x0c00;
const GICR_ICFGR1: u32 = GICR_SGI_BASE + 0x0c04;

impl RestoredVirtioMmioSection {
    fn mmio_base(&self) -> u64 {
        match self {
            Self::Macos(section) => section.mmio_base,
            Self::Linux(section) => section.mmio_base,
        }
    }

    fn transport(&self) -> &MmioTransportState {
        match self {
            Self::Macos(section) => &section.transport,
            Self::Linux(section) => &section.transport,
        }
    }

    fn transport_for_hvf(&self) -> MmioTransportState {
        self.transport().clone()
    }

    fn device(&self) -> Option<&DeviceSnapshot> {
        match self {
            Self::Macos(section) => section.device.as_ref(),
            Self::Linux(section) => section.device.as_ref(),
        }
    }

    fn device_type_name(&self) -> String {
        match self {
            Self::Macos(section) => format!("virtio-{}", section.device_type),
            Self::Linux(section) => section.device_type.clone(),
        }
    }
}

/// Inputs the orchestrator needs from the Vmm. Wired up in a dedicated method
/// so the orchestrator stays decoupled from the rest of the Vmm internals.
pub struct CaptureInputs<'a> {
    pub guest_memory: &'a GuestMemoryMmap,
    pub ram_ranges: &'a [(u64, u64)],
    pub vcpu_handles: &'a [VcpuHandle],
    pub vcpu_ids: &'a [u64],
    pub vcpu_list: &'a Arc<VcpuList>,
    pub irqchip: Option<&'a IrqChip>,
    pub gic: Option<&'a Arc<Mutex<GicV3>>>,
    pub virtio_transports: &'a [(u64, Arc<Mutex<MmioTransport>>)],
    pub nested_enabled: bool,
}

fn cntvct_el0() -> u64 {
    unsafe extern "C" {
        fn mach_absolute_time() -> u64;
    }
    unsafe { mach_absolute_time() }
}

fn deterministic_time_enabled() -> bool {
    std::env::var_os("KRUN_DETERMINISTIC_TIME").is_some_and(|value| value == "1")
}

fn restore_timer_delta(format: SnapshotFormat, capture_counter: u64) -> u64 {
    if deterministic_time_enabled() {
        crate::timing_event("snapshot.restore.deterministic_time.skip_timer_rebase");
        return 0;
    }
    match format {
        SnapshotFormat::Macos => cntvct_el0().wrapping_sub(capture_counter),
        SnapshotFormat::Linux => 0,
    }
}

fn topology_hash_for(
    ram_ranges: &[(u64, u64)],
    vcpu_count: u32,
    nested_enabled: bool,
    gic: Option<&GicTopology>,
    virtio: &[VirtioTopology],
) -> [u8; 32] {
    snapshot_metadata::compute_topology_hash(ram_ranges, vcpu_count, nested_enabled, gic, virtio)
}

fn gic_topology(inputs: &CaptureInputs<'_>) -> Option<GicTopology> {
    inputs.irqchip.map(|irqchip| {
        let irqchip = irqchip.lock().unwrap();
        GicTopology {
            compatibility: irqchip.fdt_compatibility(),
            version: irqchip.version(),
            maint_irq: irqchip.fdt_maint_irq(),
            vcpu_count: irqchip.vcpu_count(),
            properties: irqchip.device_properties(),
        }
    })
}

fn virtio_topology(inputs: &CaptureInputs<'_>) -> Vec<VirtioTopology> {
    inputs
        .virtio_transports
        .iter()
        .map(|(base, transport_arc)| {
            let transport = transport_arc.lock().unwrap();
            VirtioTopology {
                mmio_base: *base,
                device_name: transport.locked_device().device_name().to_string(),
            }
        })
        .collect()
}

fn validate_snapshot_meta(
    inputs: &CaptureInputs<'_>,
    meta: &MetaSection,
) -> Result<SnapshotFormat> {
    let source_format = meta.source_format().ok_or_else(|| {
        SnapshotError::ConfigMismatch(format!(
            "unsupported snapshot source backend {}",
            meta.source_backend
        ))
    })?;
    if meta.vcpu_count != inputs.vcpu_handles.len() as u32 {
        return Err(SnapshotError::ConfigMismatch(format!(
            "snapshot vcpu_count {} != configured {}",
            meta.vcpu_count,
            inputs.vcpu_handles.len()
        )));
    }
    if meta.nested_enabled != inputs.nested_enabled {
        return Err(SnapshotError::ConfigMismatch(
            "nested_enabled differs between snapshot and current ctx".into(),
        ));
    }
    if meta.guest_arch != GUEST_ARCH_AARCH64 {
        return Err(SnapshotError::ConfigMismatch(format!(
            "snapshot guest_arch {} != {GUEST_ARCH_AARCH64}",
            meta.guest_arch
        )));
    }
    if meta.pauth_policy != PAUTH_POLICY_NOPAUTH {
        return Err(SnapshotError::ConfigMismatch(format!(
            "snapshot pauth policy {} != {PAUTH_POLICY_NOPAUTH}",
            meta.pauth_policy
        )));
    }
    if meta.topology_hash_version != TOPOLOGY_HASH_VERSION {
        return Err(SnapshotError::ConfigMismatch(format!(
            "snapshot topology hash version {} != {TOPOLOGY_HASH_VERSION}",
            meta.topology_hash_version
        )));
    }
    let snapshot_ranges = snapshot_metadata::ram_ranges_from_layout(&meta.ram);
    let snapshot_hash = topology_hash_for(
        &snapshot_ranges,
        meta.vcpu_count,
        meta.nested_enabled,
        meta.gic_topology.as_ref(),
        &meta.virtio_topology,
    );
    if meta.topology_hash != snapshot_hash {
        return Err(SnapshotError::ConfigMismatch(format!(
            "snapshot topology hash {} does not match snapshot metadata {}",
            snapshot_metadata::hash_hex(&meta.topology_hash),
            snapshot_metadata::hash_hex(&snapshot_hash)
        )));
    }
    let current_gic = gic_topology(inputs);
    let current_virtio = virtio_topology(inputs);
    let current_hash = topology_hash_for(
        inputs.ram_ranges,
        inputs.vcpu_handles.len() as u32,
        inputs.nested_enabled,
        current_gic.as_ref(),
        &current_virtio,
    );
    if meta.topology_hash != current_hash {
        return Err(SnapshotError::ConfigMismatch(format!(
            "snapshot topology hash {} differs from current VM {}; snapshot {}; current {}",
            snapshot_metadata::hash_hex(&meta.topology_hash),
            snapshot_metadata::hash_hex(&current_hash),
            snapshot_metadata::topology_summary(
                &snapshot_ranges,
                meta.vcpu_count,
                meta.nested_enabled,
                meta.gic_topology.as_ref(),
                &meta.virtio_topology
            ),
            snapshot_metadata::topology_summary(
                inputs.ram_ranges,
                inputs.vcpu_handles.len() as u32,
                inputs.nested_enabled,
                current_gic.as_ref(),
                &current_virtio
            )
        )));
    }
    Ok(source_format)
}

/// Capture a complete snapshot into a staging directory, then publish it to `dir`.
pub fn capture(inputs: CaptureInputs<'_>, dir: &Path) -> Result<()> {
    capture_with_paused_hook(inputs, dir, |_| Ok(()))
}

/// Capture a complete snapshot into a staging directory, run `paused_hook`
/// while vCPUs and devices are still paused, then publish it to `dir`.
pub fn capture_with_paused_hook<F>(
    inputs: CaptureInputs<'_>,
    dir: &Path,
    paused_hook: F,
) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
    crate::timing_event("snapshot.capture.begin");
    // 1. Quiesce: pause all vCPUs and collect their state.
    let vcpu_states = match pause_vcpus(inputs.vcpu_handles, inputs.vcpu_ids) {
        Ok(states) => states,
        Err(e) => {
            let _ = resume_vcpus(inputs.vcpu_handles);
            return Err(e);
        }
    };
    crate::timing_event("snapshot.capture.vcpus.paused");
    let capture_mach_time = cntvct_el0();

    let result = capture_paused(&inputs, dir, &vcpu_states, capture_mach_time, paused_hook);
    crate::timing_event("snapshot.capture.paused_work.done");

    // Always attempt to resume every device and vCPU before returning. A failed
    // snapshot must not strand the caller's running VM in a paused state.
    let device_resume = resume_devices(&inputs);
    crate::timing_event("snapshot.capture.devices.resumed");
    let vcpu_resume = resume_vcpus(inputs.vcpu_handles);
    crate::timing_event("snapshot.capture.vcpus.resumed");

    result?;
    device_resume?;
    vcpu_resume?;
    crate::timing_event("snapshot.capture.complete");
    Ok(())
}

pub fn arm_dirty_tracking(inputs: &CaptureInputs<'_>) -> Result<()> {
    let vcpu_states = match pause_vcpus(inputs.vcpu_handles, inputs.vcpu_ids) {
        Ok(states) => states,
        Err(e) => {
            let _ = resume_vcpus(inputs.vcpu_handles);
            return Err(e);
        }
    };
    drop(vcpu_states);

    // The guest has been running, so RAM matches no stored image.
    let result = enable_dirty_tracking(inputs, None);
    let resume = resume_vcpus(inputs.vcpu_handles);
    result?;
    resume?;
    Ok(())
}

fn capture_paused<F>(
    inputs: &CaptureInputs<'_>,
    dir: &Path,
    vcpu_states: &[Vec<u8>],
    capture_mach_time: u64,
    paused_hook: F,
) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
    crate::timing_event("snapshot.capture_paused.begin");
    // 2. Capture transport-side state for EVERY virtio device, then attempt
    // to pause + serialize the device-specific payload. Devices that don't
    // implement snapshot reject the operation; otherwise they could continue
    // touching guest memory while RAM is being copied.
    let mut virtio_sections = Vec::new();
    for (index, (base, transport_arc)) in inputs.virtio_transports.iter().enumerate() {
        crate::timing_event(&format!(
            "snapshot.capture_paused.virtio.begin index={index} base=0x{base:x}"
        ));
        let transport = transport_arc.lock().unwrap();
        let device_type = transport.locked_device().device_type();
        let device_arc = transport.device();
        let mut device = device_arc.lock().unwrap();
        let device_snap = match device.pause() {
            Ok(()) => match device.serialize_state() {
                Ok(s) => Some(s),
                Err(devices::virtio::DeviceSnapshotError::Unsupported(e)) => {
                    return Err(SnapshotError::DeviceRefused(format!(
                        "base=0x{base:x}: {e}"
                    )));
                }
                Err(e) => {
                    return Err(SnapshotError::DeviceRefused(format!(
                        "base=0x{base:x}: {e}"
                    )));
                }
            },
            Err(devices::virtio::DeviceSnapshotError::Unsupported(e)) => {
                return Err(SnapshotError::DeviceRefused(format!(
                    "base=0x{base:x}: {e}"
                )));
            }
            Err(e) => {
                return Err(SnapshotError::DeviceRefused(format!(
                    "base=0x{base:x}: {e}"
                )));
            }
        };
        let transport_state = transport.to_state();
        drop(device);
        drop(transport);
        virtio_sections.push(VirtioMmioSection {
            mmio_base: *base,
            device_type,
            transport: transport_state,
            device: device_snap,
        });
        crate::timing_event(&format!(
            "snapshot.capture_paused.virtio.done index={index} base=0x{base:x}"
        ));
    }

    // 3. Capture GIC state.
    crate::timing_event("snapshot.capture_paused.gic.begin");
    let hvf_gic_state = match inputs.irqchip {
        Some(irqchip) => irqchip
            .lock()
            .unwrap()
            .snapshot_state()
            .map_err(|e| SnapshotError::DeviceRefused(format!("irqchip snapshot: {e:?}")))?,
        None => None,
    };
    let hvf_gic_dist_regs = match inputs.irqchip {
        Some(irqchip) => irqchip
            .lock()
            .unwrap()
            .snapshot_distributor_state()
            .map_err(|e| SnapshotError::DeviceRefused(format!("irqchip dist snapshot: {e:?}")))?,
        None => None,
    };
    let gic_state = inputs.gic.map(|g| g.lock().unwrap().to_state());
    let vcpu_list_state = inputs.vcpu_list.to_state();
    crate::timing_event("snapshot.capture_paused.gic.done");

    // 4. Write RAM.
    let stage_dir = staging_dir(dir);
    if stage_dir.exists() {
        std::fs::remove_dir_all(&stage_dir)?;
    }
    std::fs::create_dir_all(&stage_dir)?;
    crate::timing_event("snapshot.capture_paused.stage.ready");

    let result = (|| {
        crate::timing_event("snapshot.capture_paused.dirty_blocks.begin");
        // Taking the dirty set consumes the tracker's baseline until the
        // re-arm below, so if anything in between fails, the next capture
        // writes all of RAM instead of patching with blocks this one consumed.
        let dirty = hvf::take_dirty_blocks_and_reprotect()
            .map_err(|e| SnapshotError::Io(std::io::Error::other(format!("dirty RAM: {e}"))))?;
        let mut dirty_blocks = dirty.blocks;
        add_virtio_dma_dirty_blocks(
            inputs.guest_memory,
            inputs.ram_ranges,
            &virtio_sections,
            &mut dirty_blocks,
        );
        crate::timing_event(&format!(
            "snapshot.capture_paused.dirty_blocks.done count={}",
            dirty_blocks.len()
        ));
        let image_id = new_pages_image_id()?;
        crate::timing_event("snapshot.capture_paused.ram.begin");
        let ram = match incremental_base(dir, dirty.baseline) {
            Some(base) => clone_and_patch_dirty_pages_img(
                inputs.guest_memory,
                inputs.ram_ranges,
                base,
                &stage_dir,
                &dirty_blocks,
            )?,
            None => write_full_pages_img(inputs.guest_memory, inputs.ram_ranges, &stage_dir)?,
        };
        crate::timing_event("snapshot.capture_paused.ram.done");

        // 5. Assemble vmstate.bin.
        crate::timing_event("snapshot.capture_paused.vmstate.begin");
        let mut total_ram: u64 = 0;
        let mut ram_base: u64 = u64::MAX;
        for (addr, size) in inputs.ram_ranges {
            total_ram += *size;
            if *addr < ram_base {
                ram_base = *addr;
            }
        }

        let gic_topology = gic_topology(inputs);
        let virtio_topology = virtio_topology(inputs);
        let topology_hash = topology_hash_for(
            inputs.ram_ranges,
            inputs.vcpu_handles.len() as u32,
            inputs.nested_enabled,
            gic_topology.as_ref(),
            &virtio_topology,
        );
        let meta = MetaSection {
            ram,
            virtio_bases: virtio_sections.iter().map(|s| s.mmio_base).collect(),
            vcpu_count: inputs.vcpu_handles.len() as u32,
            nested_enabled: inputs.nested_enabled,
            source_backend: SOURCE_BACKEND_HVF.to_string(),
            capture_timer_counter: capture_mach_time,
            topology_hash_version: TOPOLOGY_HASH_VERSION,
            topology_hash,
            gic_topology,
            virtio_topology,
            guest_arch: GUEST_ARCH_AARCH64.to_string(),
            pauth_policy: PAUTH_POLICY_NOPAUTH.to_string(),
        };

        let mut writer = SnapshotWriter::new(total_ram, ram_base, meta.vcpu_count);
        writer.add_bincode(SectionId::Meta, 0, &meta)?;
        writer.add_bincode(SectionId::PagesImageId, 0, &image_id)?;

        for (i, bytes) in vcpu_states.iter().enumerate() {
            writer.add_raw(SectionId::Vcpu, i as u32, bytes.clone());
        }
        if let Some(gic) = &gic_state {
            writer.add_bincode(SectionId::GicDist, 0, gic)?;
        }
        if let Some(hvf_gic) = hvf_gic_state {
            writer.add_raw(SectionId::HvfGic, 0, hvf_gic);
        }
        if let Some(hvf_gic_dist_regs) = hvf_gic_dist_regs {
            writer.add_bincode(SectionId::HvfGicDistRegs, 0, &hvf_gic_dist_regs)?;
        }
        writer.add_bincode(SectionId::GicVcpu, 0, &vcpu_list_state)?;
        for (i, section) in virtio_sections.iter().enumerate() {
            writer.add_bincode(SectionId::VirtioMmio, i as u32, section)?;
        }

        writer.write_to_dir(&stage_dir)?;
        crate::timing_event("snapshot.capture_paused.vmstate.done");
        crate::timing_event("snapshot.capture_paused.paused_hook.begin");
        paused_hook(&stage_dir)?;
        crate::timing_event("snapshot.capture_paused.paused_hook.done");
        crate::timing_event("snapshot.capture_paused.publish.begin");
        publish_snapshot_dir(&stage_dir, dir)?;
        crate::timing_event("snapshot.capture_paused.publish.done");
        crate::timing_event("snapshot.capture_paused.dirty_tracking.begin");
        enable_dirty_tracking(inputs, Some(image_id))?;
        crate::timing_event("snapshot.capture_paused.dirty_tracking.done");
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_dir_all(&stage_dir);
    }

    result
}

fn resume_devices(inputs: &CaptureInputs<'_>) -> Result<()> {
    for (_base, transport_arc) in inputs.virtio_transports {
        let transport = transport_arc.lock().unwrap();
        let device_arc = transport.device();
        let mut device = device_arc.lock().unwrap();
        device
            .resume()
            .map_err(|e| SnapshotError::DeviceRefused(format!("resume: {e}")))?;
    }
    Ok(())
}

/// `baseline` is the pages.img guest RAM matches at this instant, if any.
pub(crate) fn enable_dirty_tracking(
    inputs: &CaptureInputs<'_>,
    baseline: Option<PagesImageId>,
) -> Result<()> {
    hvf::enable_dirty_tracking(inputs.ram_ranges, baseline)
        .map_err(|e| SnapshotError::Io(std::io::Error::other(format!("enable dirty RAM: {e}"))))
}

/// Identifies the contents of one pages.img. Every capture mints a new id and
/// records it in vmstate.bin next to the image; restore hands it to the dirty
/// tracker. Copies of a snapshot keep its id because they keep its contents.
pub(crate) type PagesImageId = hvf::DirtyBaseline;

fn new_pages_image_id() -> Result<PagesImageId> {
    let mut bytes = [0u8; 16];
    // SAFETY: getentropy writes at most `bytes.len()` (<= 256) bytes into
    // the buffer it is given.
    if unsafe { libc::getentropy(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(PagesImageId::from_le_bytes(bytes))
}

fn stored_pages_image_id(reader: &super::SnapshotReader) -> Option<PagesImageId> {
    reader.get_bincode(SectionId::PagesImageId, 0).ok()
}

/// The snapshot whose pages.img a capture may clone and patch with the blocks
/// dirtied since `baseline`: `dir` itself, but only when it holds exactly the
/// image `baseline` names. Anything else (no baseline, no image, an image from
/// another VM or another point in this VM's history) needs a full RAM write.
fn incremental_base(dir: &Path, baseline: Option<PagesImageId>) -> Option<&Path> {
    let Some(baseline) = baseline else {
        crate::timing_event("snapshot.capture_paused.ram.full reason=no_baseline");
        return None;
    };
    if !dir.join(super::PAGES_IMG).exists() {
        crate::timing_event("snapshot.capture_paused.ram.full reason=no_pages_img");
        return None;
    }
    let stored = super::SnapshotReader::open(dir)
        .ok()
        .and_then(|reader| stored_pages_image_id(&reader));
    if stored != Some(baseline) {
        crate::timing_event("snapshot.capture_paused.ram.full reason=image_mismatch");
        return None;
    }
    crate::timing_event("snapshot.capture_paused.ram.incremental");
    Some(dir)
}

fn add_virtio_dma_dirty_blocks(
    mem: &GuestMemoryMmap,
    ram_ranges: &[(u64, u64)],
    virtio_sections: &[VirtioMmioSection],
    dirty_blocks: &mut Vec<hvf::DirtyBlock>,
) {
    let mut ranges = Vec::new();
    for section in virtio_sections {
        let Some(device) = &section.device else {
            continue;
        };
        for queue in &device.queues {
            collect_queue_dma_ranges(mem, queue, &mut ranges);
        }
    }

    for (addr, size) in ranges {
        add_dirty_range(ram_ranges, dirty_blocks, addr, size);
    }
    dirty_blocks.sort_by_key(|block| block.guest_addr);
    dirty_blocks.dedup_by_key(|block| block.guest_addr);
    crate::timing_event(&format!(
        "snapshot.capture_paused.virtio_dma_dirty.done count={}",
        dirty_blocks.len()
    ));
}

fn collect_queue_dma_ranges(
    mem: &GuestMemoryMmap,
    queue: &QueueState,
    ranges: &mut Vec<(u64, u64)>,
) {
    if !queue.ready || queue.size == 0 {
        return;
    }

    let queue_size = u64::from(queue.size);
    ranges.push((queue.desc_table, queue_size * 16));
    ranges.push((queue.avail_ring, 4 + queue_size * 2 + 2));
    ranges.push((queue.used_ring, 4 + queue_size * 8 + 2));

    for index in 0..queue.size {
        let Some(desc_addr) = queue.desc_table.checked_add(u64::from(index) * 16) else {
            continue;
        };
        let Ok(desc) = mem.read_obj::<Descriptor>(GuestAddress(desc_addr)) else {
            continue;
        };
        if desc.len != 0 {
            ranges.push((desc.addr, u64::from(desc.len)));
        }
    }
}

fn add_dirty_range(
    ram_ranges: &[(u64, u64)],
    dirty_blocks: &mut Vec<hvf::DirtyBlock>,
    addr: u64,
    size: u64,
) {
    let Some(end) = addr.checked_add(size.saturating_sub(1)) else {
        return;
    };
    for (ram_addr, ram_size) in ram_ranges {
        let ram_end = ram_addr.saturating_add(*ram_size);
        let start = addr.max(*ram_addr);
        let end = end.min(ram_end.saturating_sub(1));
        if start > end {
            continue;
        }

        let first =
            ((start - *ram_addr) / hvf::DIRTY_BLOCK_SIZE) * hvf::DIRTY_BLOCK_SIZE + *ram_addr;
        let last = ((end - *ram_addr) / hvf::DIRTY_BLOCK_SIZE) * hvf::DIRTY_BLOCK_SIZE + *ram_addr;
        let mut block_addr = first;
        while block_addr <= last {
            dirty_blocks.push(hvf::DirtyBlock {
                guest_addr: block_addr,
                size: hvf::DIRTY_BLOCK_SIZE.min(ram_end - block_addr),
            });
            let Some(next) = block_addr.checked_add(hvf::DIRTY_BLOCK_SIZE) else {
                break;
            };
            block_addr = next;
        }
    }
}

fn staging_dir(dir: &Path) -> PathBuf {
    let name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("snapshot");
    let stage_name = format!(".{name}.tmp.{}", std::process::id());
    match dir.parent() {
        Some(parent) => parent.join(stage_name),
        None => PathBuf::from(stage_name),
    }
}

fn publish_snapshot_dir(stage_dir: &Path, dir: &Path) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::rename(stage_dir, dir)?;
    Ok(())
}

/// Sends Pause to every vCPU, forces them out of hv_vcpu_run, and collects
/// their serialized state.
fn pause_vcpus(handles: &[VcpuHandle], vcpu_ids: &[u64]) -> Result<Vec<Vec<u8>>> {
    let tickets = send_to_all_vcpus(handles, || VcpuEvent::Pause)?;
    // Kick each vCPU so it returns from hv_vcpu_run and picks up the event.
    for &id in vcpu_ids {
        let _ = hvf::vcpu_request_exit(id);
    }
    let mut states = Vec::with_capacity(handles.len());
    for (i, (h, ticket)) in handles.iter().zip(tickets).enumerate() {
        match vcpu_reply(h, i, ticket, "pause")? {
            VcpuResponse::Paused(bytes) => states.push(bytes),
            other => return Err(unexpected_vcpu_reply(i, "pause", other)),
        }
    }
    Ok(states)
}

pub fn resume_vcpus(handles: &[VcpuHandle]) -> Result<()> {
    let tickets = send_to_all_vcpus(handles, || VcpuEvent::Resume)?;
    for (i, (h, ticket)) in handles.iter().zip(tickets).enumerate() {
        match vcpu_reply(h, i, ticket, "resume")? {
            VcpuResponse::Resumed => {}
            other => return Err(unexpected_vcpu_reply(i, "resume", other)),
        }
    }
    Ok(())
}

fn send_to_all_vcpus(
    handles: &[VcpuHandle],
    event: impl Fn() -> VcpuEvent,
) -> Result<Vec<VcpuTicket>> {
    handles
        .iter()
        .enumerate()
        .map(|(i, h)| h.send_event(event()).map_err(|e| vcpu_error(i, "send", e)))
        .collect()
}

/// Sends `event` to one paused vCPU and waits for the reply.
fn vcpu_call(
    handle: &VcpuHandle,
    index: usize,
    event: VcpuEvent,
    what: &str,
) -> Result<VcpuResponse> {
    let ticket = handle
        .send_event(event)
        .map_err(|e| vcpu_error(index, what, format!("send: {e}")))?;
    vcpu_reply(handle, index, ticket, what)
}

/// Waits for the reply to `ticket`. Replies left over from earlier requests
/// that timed out are skipped, so they can never be mistaken for this one.
fn vcpu_reply(
    handle: &VcpuHandle,
    index: usize,
    ticket: VcpuTicket,
    what: &str,
) -> Result<VcpuResponse> {
    match handle.wait_reply(ticket, Duration::from_millis(VCPU_PAUSE_TIMEOUT_MS)) {
        Ok(VcpuResponse::Error(e)) => Err(vcpu_error(index, what, e)),
        Ok(response) => Ok(response),
        Err(e) => Err(vcpu_error(index, what, e)),
    }
}

fn unexpected_vcpu_reply(index: usize, what: &str, response: VcpuResponse) -> SnapshotError {
    vcpu_error(index, what, format!("unexpected response {response:?}"))
}

fn vcpu_error(index: usize, what: &str, detail: impl std::fmt::Display) -> SnapshotError {
    SnapshotError::Io(std::io::Error::other(format!(
        "vcpu {index}: {what}: {detail}"
    )))
}

/// Restore-side: given a fully-built (post-activate but pre-vCPU-run) VMM and
/// a SnapshotReader, push the captured state into vCPUs, GIC, and devices,
/// then re-arm the virtual timer. Caller has already constructed memory from
/// `pages.img`, so guest RAM is in place.
pub fn restore(inputs: &CaptureInputs<'_>, reader: &super::SnapshotReader) -> Result<()> {
    info!("snapshot restore: starting");
    crate::timing_event("snapshot.restore.begin");
    let meta: MetaSection = reader.get_bincode(SectionId::Meta, 0)?;
    crate::timing_event("snapshot.restore.meta.loaded");
    info!(
        "snapshot restore: meta loaded — vcpu_count={}, ram={} bytes, virtio_devs={}",
        meta.vcpu_count,
        meta.ram.regions.iter().map(|r| r.size).sum::<u64>(),
        meta.virtio_bases.len()
    );
    let source_format = validate_snapshot_meta(inputs, &meta)?;
    crate::timing_event("snapshot.restore.config.checked");

    let linux_gic = if source_format == SnapshotFormat::Linux {
        restore_linux_gic_state(reader, inputs.vcpu_handles.len())?
    } else {
        None
    };

    // vCPUs were pre-paused by the builder (queue_initial_pause), so they're
    // already blocked at the top of their first loop iteration. Drain their
    // initial Paused responses before sending RestoreState.
    for (i, h) in inputs.vcpu_handles.iter().enumerate() {
        match vcpu_reply(h, i, h.initial_pause_ticket(), "initial pause")? {
            VcpuResponse::Paused(_) => {}
            other => return Err(unexpected_vcpu_reply(i, "initial pause", other)),
        }
        crate::timing_event(&format!("snapshot.restore.vcpu.initial_paused index={i}"));
    }

    // Restore GIC state.
    crate::timing_event("snapshot.restore.irqchip.begin");
    if let (Some(irqchip), Some(linux_gic)) = (inputs.irqchip, &linux_gic) {
        irqchip
            .lock()
            .unwrap()
            .restore_linux_gic_dist_state(&linux_gic.dist_regs, LinuxGicDistRestorePhase::Ctlr)
            .map_err(|e| {
                SnapshotError::DeviceRefused(format!("linux GIC distributor CTLR restore: {e:?}"))
            })?;
        crate::timing_event("snapshot.restore.linux_gic.dist_ctlr.done");
    }
    if source_format == SnapshotFormat::Macos
        && std::env::var_os("KRUN_SKIP_MACOS_GIC_RESTORE").is_none()
        && let Some(irqchip) = inputs.irqchip
    {
        if let Ok(st) = reader.get_raw(SectionId::HvfGic, 0) {
            irqchip
                .lock()
                .unwrap()
                .restore_snapshot_state(st)
                .map_err(|e| SnapshotError::DeviceRefused(format!("irqchip restore: {e:?}")))?;
        }
    }
    crate::timing_event("snapshot.restore.irqchip.done");
    if let Some(gic) = inputs.gic {
        if let Ok(st) = reader.get_bincode::<GicV3State>(SectionId::GicDist, 0) {
            gic.lock().unwrap().restore_state(&st);
        }
    }
    if let Ok(st) = reader.get_bincode::<VcpuListState>(SectionId::GicVcpu, 0) {
        inputs.vcpu_list.restore_state(&st);
    }
    crate::timing_event("snapshot.restore.gic.done");

    if let Some(linux_gic) = &linux_gic {
        for (i, h) in inputs.vcpu_handles.iter().enumerate() {
            let redist_regs = linux_gic
                .vcpus
                .get(i)
                .map(|state| state.redist_regs.clone())
                .unwrap_or_default();
            let what = "redist restore";
            match vcpu_call(h, i, VcpuEvent::RestoreGicRedist(redist_regs), what)? {
                VcpuResponse::Restored => {}
                other => return Err(unexpected_vcpu_reply(i, what, other)),
            }
            crate::timing_event(&format!("snapshot.restore.linux_gic.redist.done index={i}"));
        }
    }

    // QEMU's HVF VGIC restore writes GICD_CTLR first, then per-vCPU
    // redistributor/ICC state, then the shared distributor state. Linux-origin
    // snapshots follow the same order here so pending shared SPIs are routed
    // after the target CPU interfaces are programmed.
    for (i, h) in inputs.vcpu_handles.iter().enumerate() {
        crate::timing_event(&format!("snapshot.restore.vcpu.state.begin index={i}"));
        let bytes = reader.get_raw(SectionId::Vcpu, i as u32)?.to_vec();
        let event = match source_format {
            SnapshotFormat::Macos => VcpuEvent::RestoreState(bytes),
            SnapshotFormat::Linux => VcpuEvent::RestoreKvmState {
                state: bytes,
                restore_counter: cntvct_el0(),
                gic: linux_gic.as_ref().and_then(|gic| {
                    gic.vcpus.get(i).cloned().map(|mut state| {
                        state.redist_regs.clear();
                        state
                    })
                }),
            },
        };
        match vcpu_call(h, i, event, "restore")? {
            VcpuResponse::Restored => {}
            other => return Err(unexpected_vcpu_reply(i, "restore", other)),
        }
        crate::timing_event(&format!("snapshot.restore.vcpu.state.done index={i}"));
    }

    if let (Some(irqchip), Some(linux_gic)) = (inputs.irqchip, &linux_gic) {
        irqchip
            .lock()
            .unwrap()
            .restore_linux_gic_dist_state(&linux_gic.dist_regs, LinuxGicDistRestorePhase::Shared)
            .map_err(|e| {
                SnapshotError::DeviceRefused(format!("linux GIC distributor shared restore: {e:?}"))
            })?;
        crate::timing_event("snapshot.restore.linux_gic.dist_shared.done");
    }

    if let (Some(irqchip), Some(linux_gic)) = (inputs.irqchip, &linux_gic) {
        for irq in &linux_gic.pending_spis {
            irqchip
                .lock()
                .unwrap()
                .set_irq(Some(*irq), None)
                .map_err(|e| {
                    SnapshotError::DeviceRefused(format!("linux GIC pending SPI {irq}: {e:?}"))
                })?;
        }
    }
    crate::timing_event("snapshot.restore.linux_gic.pending_spis.done");

    let timer_delta = restore_timer_delta(source_format, meta.capture_timer_counter);
    for (i, h) in inputs.vcpu_handles.iter().enumerate() {
        crate::timing_event(&format!("snapshot.restore.vcpu.timer.begin index={i}"));
        match vcpu_call(h, i, VcpuEvent::RebaseTimer(timer_delta), "timer rebase")? {
            VcpuResponse::TimerRebased => {}
            other => return Err(unexpected_vcpu_reply(i, "timer rebase", other)),
        }
        crate::timing_event(&format!("snapshot.restore.vcpu.timer.done index={i}"));
    }

    crate::timing_event("snapshot.restore.dirty_tracking.begin");
    // Guest RAM was mapped from this snapshot's pages.img and no vCPU has run
    // yet, so RAM matches that image exactly.
    enable_dirty_tracking(inputs, stored_pages_image_id(reader))?;
    crate::timing_event("snapshot.restore.dirty_tracking.done");

    // Restore virtio devices by MMIO base rather than by vector index so
    // optional devices cannot shift the mapping.
    let mut restored_transports = Vec::new();
    for i in 0..meta.virtio_bases.len() {
        crate::timing_event(&format!("snapshot.restore.virtio.begin index={i}"));
        let section = read_virtio_section(reader, source_format, i as u32)?;
        let transport_arc = inputs
            .virtio_transports
            .iter()
            .find_map(|(b, t)| {
                if *b == section.mmio_base() {
                    Some(t)
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                SnapshotError::ConfigMismatch(format!(
                    "no virtio device at base 0x{:x} in current ctx",
                    section.mmio_base()
                ))
            })?;
        let transport_state = section.transport_for_hvf();
        {
            let mut transport = transport_arc.lock().unwrap();
            let live_device_name = transport.locked_device().device_name().to_string();
            let snapshot_device_name = section.device_type_name();
            crate::timing_event(&format!(
                "snapshot.restore.virtio.match index={i} base=0x{:x} snapshot={} live={}",
                section.mmio_base(),
                snapshot_device_name,
                live_device_name
            ));
            if source_format == SnapshotFormat::Linux && live_device_name != snapshot_device_name {
                return Err(SnapshotError::ConfigMismatch(format!(
                    "virtio device mismatch base=0x{:x}: snapshot={} live={}",
                    section.mmio_base(),
                    snapshot_device_name,
                    live_device_name
                )));
            }
            if let Some(device_snap) = section.device() {
                crate::timing_event(&format!(
                    "snapshot.restore.virtio.transport_restore.begin index={i} base=0x{:x} queues={}",
                    section.mmio_base(),
                    device_snap.queues.len()
                ));
                transport
                    .restore_queues_and_activate(&transport_state, &device_snap.queues)
                    .map_err(|e| {
                        SnapshotError::DeviceRefused(format!(
                            "base=0x{:x}: activate: {e}",
                            section.mmio_base()
                        ))
                    })?;
                crate::timing_event(&format!(
                    "snapshot.restore.virtio.transport_restore.done index={i} base=0x{:x}",
                    section.mmio_base()
                ));
            } else {
                crate::timing_event(&format!(
                    "snapshot.restore.virtio.transport_state.begin index={i} base=0x{:x}",
                    section.mmio_base()
                ));
                transport.restore_state(&transport_state);
                crate::timing_event(&format!(
                    "snapshot.restore.virtio.transport_state.done index={i} base=0x{:x}",
                    section.mmio_base()
                ));
            }
        }
        if let Some(device_snap) = section.device() {
            let transport = transport_arc.lock().unwrap();
            let device_arc = transport.device();
            let mut device = device_arc.lock().unwrap();
            crate::timing_event(&format!(
                "snapshot.restore.virtio.device_pause.begin index={i} base=0x{:x}",
                section.mmio_base()
            ));
            if let Err(e) = device.pause() {
                return Err(SnapshotError::DeviceRefused(format!(
                    "base=0x{:x}: pause: {e}",
                    section.mmio_base()
                )));
            }
            crate::timing_event(&format!(
                "snapshot.restore.virtio.device_restore.begin index={i} base=0x{:x}",
                section.mmio_base()
            ));
            let restore_result = match section {
                RestoredVirtioMmioSection::Macos(_) => device.restore_state(device_snap),
                RestoredVirtioMmioSection::Linux(_) => device.restore_macos_state(device_snap),
            };
            restore_result.map_err(|e| {
                SnapshotError::DeviceRefused(format!(
                    "base=0x{:x}: restore: {e}",
                    section.mmio_base()
                ))
            })?;
            crate::timing_event(&format!(
                "snapshot.restore.virtio.device_resume.begin index={i} base=0x{:x}",
                section.mmio_base()
            ));
            device.resume_after_restore().map_err(|e| {
                SnapshotError::DeviceRefused(format!(
                    "base=0x{:x}: resume: {e}",
                    section.mmio_base()
                ))
            })?;
            crate::timing_event(&format!(
                "snapshot.restore.virtio.device_resume.done index={i} base=0x{:x}",
                section.mmio_base()
            ));
        }
        let device_name = {
            let transport = transport_arc.lock().unwrap();
            transport.locked_device().device_name().to_string()
        };
        restored_transports.push((device_name, transport_arc.clone()));
        transport_arc.lock().unwrap().replay_pending_interrupt();
        crate::timing_event(&format!(
            "snapshot.restore.virtio.done index={i} base=0x{:x}",
            section.mmio_base()
        ));
    }

    post_restore_devices(inputs)?;
    for (_, transport) in &restored_transports {
        transport.lock().unwrap().replay_queue_notifications();
    }
    for (_, transport) in inputs.virtio_transports {
        transport.lock().unwrap().replay_pending_interrupt();
    }
    crate::timing_event("snapshot.restore.interrupts.replayed");
    info!("snapshot restore: complete");
    crate::timing_event("snapshot.restore.complete");

    Ok(())
}

fn post_restore_devices(inputs: &CaptureInputs<'_>) -> Result<()> {
    for (base, transport_arc) in inputs.virtio_transports {
        let transport = transport_arc.lock().unwrap();
        let device_arc = transport.device();
        let mut device = device_arc.lock().unwrap();
        device.post_restore().map_err(|e| {
            SnapshotError::DeviceRefused(format!("base=0x{base:x}: post restore: {e}"))
        })?;
    }
    Ok(())
}

fn restore_linux_gic_state(
    reader: &super::SnapshotReader,
    configured_vcpus: usize,
) -> Result<Option<RestoredLinuxGicState>> {
    let bytes = match reader.get_raw(SectionId::HvfGic, 0) {
        Ok(bytes) => bytes,
        Err(SnapshotError::SectionMissing { .. }) => return Ok(None),
        Err(e) => return Err(e),
    };
    let snapshot: KvmGicV3SnapshotCompat = bincode::deserialize(bytes)?;
    if snapshot.vcpu_count != configured_vcpus as u64 {
        return Err(SnapshotError::ConfigMismatch(format!(
            "linux GIC vcpu_count {} != configured {}",
            snapshot.vcpu_count, configured_vcpus
        )));
    }

    let mut restored = RestoredLinuxGicState {
        vcpus: vec![KvmGicVcpuState::default(); configured_vcpus],
        dist_regs: Vec::new(),
        pending_spis: Vec::new(),
    };

    for reg in &snapshot.regs64 {
        if reg.group != KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS {
            continue;
        }
        let vcpu = kvm_gic_attr_mpidr(reg.attr) as usize;
        if let Some(state) = restored.vcpus.get_mut(vcpu) {
            let offset = kvm_gic_attr_offset(reg.attr);
            // KVM saves four active-priority registers per group; HVF has
            // one, so keep only the registers HVF snapshots carry.
            if let Some(hvf_reg) = kvm_cpu_sysreg_to_hvf_ich_reg(offset) {
                if hvf::state::is_snapshot_gic_ich_reg(hvf_reg) {
                    state.ich_regs.push((hvf_reg, reg.value));
                }
            } else if !kvm_cpu_sysreg_is_icc_apr(offset)
                && hvf::state::GIC_ICC_REGS.contains(&(offset as u16))
            {
                state.icc_regs.push((offset as u16, reg.value));
            }
        }
    }

    for reg in &snapshot.regs32 {
        let offset = kvm_gic_attr_offset(reg.attr) as u32;
        if reg.group == KVM_DEV_ARM_VGIC_GRP_REDIST_REGS {
            let vcpu = kvm_gic_attr_mpidr(reg.attr) as usize;
            if let (Some(state), Some(hvf_reg)) = (
                restored.vcpus.get_mut(vcpu),
                kvm_redist_offset_to_hvf_reg(offset),
            ) {
                state.redist_regs.push((hvf_reg, u64::from(reg.value)));
            }
        }

        if reg.group == KVM_DEV_ARM_VGIC_GRP_DIST_REGS {
            restored.dist_regs.push(LinuxGicDistReg {
                group: reg.group,
                attr: reg.attr,
                value: reg.value,
            });
            collect_pending_spis(offset, reg.value, &mut restored.pending_spis);
        }
    }

    restored.pending_spis.sort_unstable();
    restored.pending_spis.dedup();
    Ok(Some(restored))
}

fn kvm_gic_attr_offset(attr: u64) -> u64 {
    attr & KVM_DEV_ARM_VGIC_OFFSET_MASK
}

fn kvm_gic_attr_mpidr(attr: u64) -> u64 {
    attr >> KVM_DEV_ARM_VGIC_V3_MPIDR_SHIFT
}

const fn kvm_vgic_sysreg(op0: u64, op1: u64, crn: u64, crm: u64, op2: u64) -> u64 {
    (op0 << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2
}

fn kvm_cpu_sysreg_to_hvf_ich_reg(offset: u64) -> Option<u16> {
    let is_ich_apr = ICH_AP0R_EL2
        .iter()
        .chain(ICH_AP1R_EL2.iter())
        .any(|&reg| reg == offset);
    let is_ich_lr = (ICH_LR0_EL2..ICH_LR0_EL2 + 8).contains(&offset)
        || (ICH_LR8_EL2..ICH_LR8_EL2 + 8).contains(&offset);
    if offset == ICH_VMCR_EL2 || offset == ICH_HCR_EL2 || is_ich_lr || is_ich_apr {
        u16::try_from(offset).ok()
    } else {
        None
    }
}

fn kvm_cpu_sysreg_is_icc_apr(offset: u64) -> bool {
    ICC_AP0R_EL1
        .iter()
        .chain(ICC_AP1R_EL1.iter())
        .any(|&reg| reg == offset)
}

fn kvm_redist_offset_to_hvf_reg(offset: u32) -> Option<u32> {
    match offset {
        GICR_IGROUPR0 | GICR_ISENABLER0 | GICR_ISPENDR0 | GICR_ISACTIVER0 | GICR_ICFGR0
        | GICR_ICFGR1 => Some(offset),
        offset if (GICR_IPRIORITYR..GICR_IPRIORITYR + 32).contains(&offset) => Some(offset),
        _ => None,
    }
}

fn collect_pending_spis(offset: u32, value: u32, pending: &mut Vec<u32>) {
    if !(GICD_ISPENDR..GICD_ISPENDR + 0x80).contains(&offset) {
        return;
    }
    let base_irq = (offset - GICD_ISPENDR) * 8;
    for bit in 0..32 {
        if (value & (1 << bit)) == 0 {
            continue;
        }
        let irq = base_irq + bit;
        if (GIC_INTERNAL..=arch::aarch64::layout::IRQ_MAX).contains(&irq) {
            pending.push(irq);
        }
    }
}

fn read_virtio_section(
    reader: &super::SnapshotReader,
    source_format: SnapshotFormat,
    index: u32,
) -> Result<RestoredVirtioMmioSection> {
    match source_format {
        SnapshotFormat::Macos => reader
            .get_bincode(SectionId::VirtioMmio, index)
            .map(RestoredVirtioMmioSection::Macos),
        SnapshotFormat::Linux => reader
            .get_bincode(SectionId::VirtioMmio, index)
            .map(RestoredVirtioMmioSection::Linux),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport_state() -> MmioTransportState {
        MmioTransportState {
            features_select: 1,
            acked_features_select: 2,
            queue_select: 3,
            device_status: 4,
            config_generation: 5,
            shm_region_select: 6,
            interrupt_status: 7,
            irq_line: Some(40),
        }
    }

    #[test]
    fn linux_transport_for_hvf_preserves_guest_irq_line() {
        let section = LinuxVirtioMmioSection {
            mmio_base: 0x1000_0000,
            device_type: "block".to_string(),
            transport: transport_state(),
            device: None,
        };
        let restored = RestoredVirtioMmioSection::Linux(section);

        assert_eq!(restored.transport_for_hvf().irq_line, Some(40));
    }

    #[test]
    fn reads_linux_virtio_section_with_string_device_type() {
        let dir = std::env::temp_dir().join(format!(
            "lnx-macos-linux-virtio-section-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let section = LinuxVirtioMmioSection {
            mmio_base: 0x1000_0000,
            device_type: "block".to_string(),
            transport: transport_state(),
            device: None,
        };
        let mut writer = SnapshotWriter::new(0x4000_0000, 0x8000_0000, 1);
        writer
            .add_bincode(SectionId::VirtioMmio, 0, &section)
            .expect("add virtio");
        writer.write_to_dir(&dir).expect("write");

        let reader = super::super::SnapshotReader::open(&dir).expect("open");
        let decoded = read_virtio_section(&reader, SnapshotFormat::Linux, 0).expect("decode");
        match decoded {
            RestoredVirtioMmioSection::Linux(decoded) => {
                assert_eq!(decoded.mmio_base, section.mmio_base);
                assert_eq!(decoded.device_type, "block");
                assert_eq!(decoded.transport.device_status, 4);
            }
            RestoredVirtioMmioSection::Macos(_) => panic!("expected linux section"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Temporary directory holding one snapshot directory, removed on drop.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "lnx-macos-snapshot-{name}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Self(dir)
        }

        fn snapshot(&self) -> PathBuf {
            self.0.join("snap")
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_snapshot(dir: &Path, image_id: Option<PagesImageId>, pages: &[u8]) {
        std::fs::create_dir_all(dir).expect("create snapshot dir");
        let mut writer = SnapshotWriter::new(0, 0, 0);
        if let Some(image_id) = image_id {
            writer
                .add_bincode(SectionId::PagesImageId, 0, &image_id)
                .expect("add image id");
        }
        writer.write_to_dir(dir).expect("write vmstate");
        std::fs::write(super::super::pages_img_path(dir), pages).expect("write pages");
    }

    fn read_pages(dir: &Path) -> Vec<u8> {
        std::fs::read(super::super::pages_img_path(dir)).expect("read pages")
    }

    fn read_image_id(dir: &Path) -> Option<PagesImageId> {
        stored_pages_image_id(&super::super::SnapshotReader::open(dir).expect("open"))
    }

    #[test]
    fn incremental_base_requires_the_tracked_image() {
        let scratch = ScratchDir::new("base-match");
        let snapshot = scratch.snapshot();
        write_snapshot(&snapshot, Some(7), b"pages");

        assert_eq!(
            incremental_base(&snapshot, Some(7)),
            Some(snapshot.as_path())
        );
        assert_eq!(incremental_base(&snapshot, Some(8)), None);
        assert_eq!(incremental_base(&snapshot, None), None);
    }

    #[test]
    fn incremental_base_rejects_snapshot_without_image_id() {
        let scratch = ScratchDir::new("base-legacy");
        let snapshot = scratch.snapshot();
        write_snapshot(&snapshot, None, b"pages");

        assert_eq!(incremental_base(&snapshot, Some(7)), None);
    }

    #[test]
    fn incremental_base_requires_pages_img() {
        let scratch = ScratchDir::new("base-no-pages");
        let snapshot = scratch.snapshot();
        write_snapshot(&snapshot, Some(7), b"pages");
        std::fs::remove_file(super::super::pages_img_path(&snapshot)).expect("remove pages");

        assert_eq!(incremental_base(&snapshot, Some(7)), None);
    }

    #[test]
    fn pages_image_ids_are_unique() {
        assert_ne!(
            new_pages_image_id().expect("first id"),
            new_pages_image_id().expect("second id")
        );
    }

    /// Serializes tests that drive the process-wide HVF dirty tracker. They
    /// use a VM without RAM ranges, so arming the tracker needs no HVF VM, a
    /// full capture writes an empty pages.img, and an incremental capture
    /// leaves the cloned base image's bytes untouched.
    static DIRTY_TRACKER_LOCK: Mutex<()> = Mutex::new(());

    struct RamlessVm {
        memory: GuestMemoryMmap,
        vcpu_list: Arc<VcpuList>,
    }

    impl RamlessVm {
        fn new() -> Self {
            Self {
                memory: GuestMemoryMmap::from_ranges(&[(GuestAddress(0x8000_0000), 0x1000)])
                    .expect("guest memory"),
                vcpu_list: Arc::new(VcpuList::new(0)),
            }
        }

        fn inputs(&self) -> CaptureInputs<'_> {
            CaptureInputs {
                guest_memory: &self.memory,
                ram_ranges: &[],
                vcpu_handles: &[],
                vcpu_ids: &[],
                vcpu_list: &self.vcpu_list,
                irqchip: None,
                gic: None,
                virtio_transports: &[],
                nested_enabled: false,
            }
        }

        fn arm(&self, baseline: Option<PagesImageId>) {
            enable_dirty_tracking(&self.inputs(), baseline).expect("arm dirty tracking");
        }

        fn capture(&self, dir: &Path, hook: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
            capture_paused(&self.inputs(), dir, &[], 0, hook)
        }
    }

    #[test]
    fn capture_patches_the_image_tracking_was_armed_from() {
        let _lock = DIRTY_TRACKER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let vm = RamlessVm::new();
        let scratch = ScratchDir::new("capture-chain");
        let snapshot = scratch.snapshot();
        write_snapshot(&snapshot, Some(7), b"base");
        vm.arm(Some(7));

        vm.capture(&snapshot, |_| Ok(())).expect("first capture");
        let first_id = read_image_id(&snapshot);
        assert_eq!(read_pages(&snapshot), b"base");
        assert_ne!(first_id, Some(7));
        assert_ne!(first_id, None);

        vm.capture(&snapshot, |_| Ok(())).expect("second capture");
        assert_eq!(read_pages(&snapshot), b"base");
        assert_ne!(read_image_id(&snapshot), first_id);
    }

    #[test]
    fn capture_after_failed_capture_writes_all_of_ram() {
        let _lock = DIRTY_TRACKER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let vm = RamlessVm::new();
        let scratch = ScratchDir::new("capture-failure");
        let snapshot = scratch.snapshot();
        write_snapshot(&snapshot, Some(7), b"stale");
        vm.arm(Some(7));

        let failed = vm.capture(&snapshot, |_| {
            Err(SnapshotError::DeviceRefused("injected".to_string()))
        });
        assert!(matches!(failed, Err(SnapshotError::DeviceRefused(_))));
        assert_eq!(read_pages(&snapshot), b"stale");
        assert_eq!(read_image_id(&snapshot), Some(7));

        vm.capture(&snapshot, |_| Ok(()))
            .expect("capture after failure");
        assert_eq!(read_pages(&snapshot), b"");
    }

    #[test]
    fn capture_over_another_vms_snapshot_writes_all_of_ram() {
        let _lock = DIRTY_TRACKER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let vm = RamlessVm::new();
        let scratch = ScratchDir::new("capture-foreign");
        let snapshot = scratch.snapshot();
        write_snapshot(&snapshot, Some(9), b"foreign");
        vm.arm(Some(7));

        vm.capture(&snapshot, |_| Ok(())).expect("capture");
        assert_eq!(read_pages(&snapshot), b"");
    }

    #[derive(Serialize)]
    struct KvmGicSnapshotFixture {
        vcpu_count: u64,
        regs32: Vec<KvmReg32Fixture>,
        regs64: Vec<KvmReg64Fixture>,
    }

    #[derive(Serialize)]
    struct KvmReg32Fixture {
        group: u32,
        attr: u64,
        value: u32,
    }

    #[derive(Serialize)]
    struct KvmReg64Fixture {
        group: u32,
        attr: u64,
        value: u64,
    }

    #[test]
    fn linux_gic_state_keeps_only_cpu_registers_hvf_snapshots_carry() {
        let icc_pmr_el1 = kvm_vgic_sysreg(3, 0, 4, 6, 0);
        let icc_rpr_el1 = kvm_vgic_sysreg(3, 0, 12, 11, 3);
        let cpu_reg = |attr: u64, value: u64| KvmReg64Fixture {
            group: KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS,
            attr,
            value,
        };
        let snapshot = KvmGicSnapshotFixture {
            vcpu_count: 1,
            regs32: Vec::new(),
            regs64: vec![
                cpu_reg(ICH_VMCR_EL2, 1),
                cpu_reg(ICH_AP0R_EL2[0], 2),
                cpu_reg(ICH_AP0R_EL2[1], 3),
                cpu_reg(icc_pmr_el1, 4),
                cpu_reg(icc_rpr_el1, 5),
            ],
        };
        let scratch = ScratchDir::new("linux-gic");
        let mut writer = SnapshotWriter::new(0, 0, 1);
        writer
            .add_bincode(SectionId::HvfGic, 0, &snapshot)
            .expect("add gic");
        writer.write_to_dir(&scratch.0).expect("write vmstate");
        let reader = super::super::SnapshotReader::open(&scratch.0).expect("open");

        let restored = restore_linux_gic_state(&reader, 1)
            .expect("decode")
            .expect("gic state");

        assert_eq!(
            restored.vcpus[0].ich_regs,
            vec![(ICH_VMCR_EL2 as u16, 1), (ICH_AP0R_EL2[0] as u16, 2)]
        );
        assert_eq!(restored.vcpus[0].icc_regs, vec![(icc_pmr_el1 as u16, 4)]);
    }

    #[test]
    fn deterministic_time_skips_macos_timer_rebase() {
        unsafe {
            std::env::set_var("KRUN_DETERMINISTIC_TIME", "1");
        }
        assert_eq!(restore_timer_delta(SnapshotFormat::Macos, u64::MAX), 0);
        unsafe {
            std::env::remove_var("KRUN_DETERMINISTIC_TIME");
        }
    }
}
