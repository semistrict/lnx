// Snapshot/restore orchestrator for the HVF backend.
//
// Capture flow:
//   1. Send Pause to every vCPU. Force-exit via hv_vcpus_exit so the vCPU
//      thread returns from hv_vcpu_run and processes the event.
//   2. Each vCPU thread serializes its HvfVcpuState and replies Paused(bytes).
//   3. Walk virtio MMIO transports: pause() each underlying device, then
//      serialize_state(). Collect MmioTransportState too.
//   4. Capture GICv3 state (distributor + per-vCPU pending IRQ bitmaps).
//   5. Write or clone-and-patch pages.img from guest memory.
//   6. Assemble vmstate.bin (META + per-vcpu + GICDIST + GICVCPU + per-virtio).
//   7. Atomic publish: write into a staging directory, rename to <path>.
//   8. Resume devices, then vCPUs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, LazyLock, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use log::info;

use crossbeam_channel::RecvTimeoutError;
use devices::legacy::{
    GicV3, GicV3State, IrqChip, LinuxGicDistReg, LinuxGicDistRestorePhase, VcpuList, VcpuListState,
};
use devices::virtio::{Descriptor, DeviceSnapshot, MmioTransport, MmioTransportState, QueueState};
use serde::{Deserialize, Serialize};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use crate::vstate::KvmGicVcpuState;

use super::container::{SectionId, SnapshotFormat, SnapshotWriter};
use super::ram::{
    RamLayout, clone_and_patch_dirty_pages_img, clone_pages_image, patch_dirty_pages_img,
    write_full_pages_img,
};
use super::{Result, SnapshotError};

const VCPU_PAUSE_TIMEOUT_MS: u64 = 2000;
const PREPATCH_ENV: &str = "KRUN_SNAPSHOT_PREPATCH";
const PREPATCH_POLL_MS: u64 = 25;
const PREPATCH_COLD_DELAY_MS: u64 = 25;
const PREPATCH_MAX_BACKOFF_MS: u64 = 250;

static PREPATCH_WORKERS: LazyLock<Mutex<HashMap<PathBuf, PrepatchWorker>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct PrepatchWorker {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

/// Top-level meta section: layout + acked-features-ish per-device summary
/// (the per-device payloads carry the device-specific detail).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetaSection {
    pub ram: RamLayout,
    /// MMIO base addresses, in registration order. The orchestrator emits
    /// one VirtioMmio section per entry with `index = position in this list`.
    pub virtio_bases: Vec<u64>,
    pub vcpu_count: u32,
    pub nested_enabled: bool,
    /// `CNTVCT_EL0` at capture, for timer re-arm on restore.
    pub capture_mach_time: u64,
}

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
    pub vcpu_handles: &'a [crate::vstate::VcpuHandle],
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

    let prepatch_dir =
        match stop_prepatch_workers_for_capture(dir, inputs.guest_memory, inputs.ram_ranges) {
            Ok(prepatch_dir) => prepatch_dir,
            Err(e) => {
                let _ = resume_vcpus(inputs.vcpu_handles);
                return Err(e);
            }
        };

    let result = capture_paused(
        &inputs,
        dir,
        prepatch_dir.as_deref(),
        &vcpu_states,
        capture_mach_time,
        paused_hook,
    );
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

    let result = enable_dirty_tracking(inputs);
    let resume = resume_vcpus(inputs.vcpu_handles);
    result?;
    resume?;
    Ok(())
}

fn capture_paused<F>(
    inputs: &CaptureInputs<'_>,
    dir: &Path,
    prepatch_dir: Option<&Path>,
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
        let mut dirty_blocks = hvf::take_dirty_blocks_and_reprotect()
            .map_err(|e| SnapshotError::Io(std::io::Error::other(format!("dirty RAM: {e}"))))?;
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
        crate::timing_event("snapshot.capture_paused.ram.begin");
        let ram = if let Some(prepatch_dir) = prepatch_dir {
            crate::timing_event("snapshot.capture_paused.ram.prepatch_base");
            clone_and_patch_dirty_pages_img(
                inputs.guest_memory,
                inputs.ram_ranges,
                prepatch_dir,
                &stage_dir,
                &dirty_blocks,
            )?
        } else if dir.join(super::PAGES_IMG).exists() {
            clone_and_patch_dirty_pages_img(
                inputs.guest_memory,
                inputs.ram_ranges,
                dir,
                &stage_dir,
                &dirty_blocks,
            )?
        } else {
            write_full_pages_img(inputs.guest_memory, inputs.ram_ranges, &stage_dir)?
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

        let meta = MetaSection {
            ram,
            virtio_bases: virtio_sections.iter().map(|s| s.mmio_base).collect(),
            vcpu_count: inputs.vcpu_handles.len() as u32,
            nested_enabled: inputs.nested_enabled,
            capture_mach_time,
        };

        let mut writer = SnapshotWriter::new(total_ram, ram_base, meta.vcpu_count);
        writer.add_bincode(SectionId::Meta, 0, &meta)?;

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
        enable_dirty_tracking(inputs)?;
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

fn enable_dirty_tracking(inputs: &CaptureInputs<'_>) -> Result<()> {
    hvf::enable_dirty_tracking(inputs.ram_ranges)
        .map_err(|e| SnapshotError::Io(std::io::Error::other(format!("enable dirty RAM: {e}"))))
}

fn prepatch_enabled() -> bool {
    std::env::var(PREPATCH_ENV)
        .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
        .unwrap_or(true)
}

fn prepatch_dir(dir: &Path) -> PathBuf {
    let name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("snapshot");
    match dir.parent() {
        Some(parent) => parent.join(format!(".{name}.prepatch")),
        None => PathBuf::from(format!(".{name}.prepatch")),
    }
}

fn start_prepatch_worker(dir: &Path, mem: &GuestMemoryMmap, ram_ranges: &[(u64, u64)]) {
    if !prepatch_enabled() || !dir.join(super::PAGES_IMG).exists() {
        return;
    }
    let dir = dir.to_path_buf();
    let stage_dir = prepatch_dir(&dir);
    let mem = mem.clone();
    let ram_ranges = ram_ranges.to_vec();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let worker_key = dir.clone();

    if let Ok(mut workers) = PREPATCH_WORKERS.lock() {
        if let Some(worker) = workers.remove(&dir) {
            worker.stop.store(true, Ordering::SeqCst);
            let _ = worker.handle.join();
        }
        let handle = thread::spawn(move || {
            if let Err(e) = run_prepatch_worker(&dir, &stage_dir, &mem, &ram_ranges, thread_stop) {
                crate::timing_event(&format!("snapshot.prepatch.error {e}"));
            }
        });
        workers.insert(worker_key, PrepatchWorker { stop, handle });
    }
}

fn stop_prepatch_workers_for_capture(
    dir: &Path,
    mem: &GuestMemoryMmap,
    ram_ranges: &[(u64, u64)],
) -> Result<Option<PathBuf>> {
    let workers = {
        let mut workers = PREPATCH_WORKERS.lock().unwrap();
        if let Some(worker) = workers.remove(dir) {
            vec![(dir.to_path_buf(), worker)]
        } else {
            workers.drain().collect::<Vec<_>>()
        }
    };
    if workers.is_empty() {
        return Ok(None);
    }

    let mut stage_dirs = Vec::with_capacity(workers.len());
    for (worker_dir, worker) in workers {
        worker.stop.store(true, Ordering::SeqCst);
        let _ = worker.handle.join();
        let stage_dir = prepatch_dir(&worker_dir);
        if stage_dir.join(super::PAGES_IMG).exists() {
            stage_dirs.push(stage_dir);
        }
    }
    if stage_dirs.is_empty() {
        return Ok(None);
    }

    let dirty = hvf::take_dirty_blocks_and_reprotect().map_err(|e| {
        SnapshotError::Io(std::io::Error::other(format!(
            "prepatch final dirty RAM: {e}"
        )))
    })?;
    if !dirty.is_empty() {
        crate::timing_event(&format!(
            "snapshot.prepatch.final_flush.begin count={}",
            dirty.len()
        ));
        for stage_dir in &stage_dirs {
            patch_dirty_pages_img(mem, ram_ranges, stage_dir, &dirty)?;
        }
        crate::timing_event("snapshot.prepatch.final_flush.done");
    }
    crate::timing_event(&format!(
        "snapshot.prepatch.capture_base count={}",
        stage_dirs.len()
    ));
    Ok(stage_dirs.into_iter().next())
}

fn run_prepatch_worker(
    dir: &Path,
    stage_dir: &Path,
    mem: &GuestMemoryMmap,
    ram_ranges: &[(u64, u64)],
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let _ = std::fs::remove_dir_all(stage_dir);
    clone_pages_image(dir, stage_dir)?;
    crate::timing_event("snapshot.prepatch.started");

    let mut pending: HashMap<u64, PendingBlock> = HashMap::new();
    let mut copied = 0usize;
    let mut delayed = 0usize;
    while !stop.load(Ordering::SeqCst) {
        let now = Instant::now();
        let dirty = hvf::take_dirty_blocks_and_reprotect().map_err(|e| {
            SnapshotError::Io(std::io::Error::other(format!("prepatch dirty RAM: {e}")))
        })?;
        for block in dirty {
            let entry = pending
                .entry(block.guest_addr)
                .or_insert_with(|| PendingBlock::new(block));
            entry.block = block;
            entry.redirties = entry.redirties.saturating_add(1);
            entry.due = now + entry.backoff();
        }

        let mut ready = Vec::new();
        pending.retain(|_, entry| {
            if entry.due <= now {
                ready.push(entry.block);
                false
            } else {
                true
            }
        });
        if !ready.is_empty() {
            copied += ready.len();
            patch_dirty_pages_img(mem, ram_ranges, stage_dir, &ready)?;
        } else if !pending.is_empty() {
            delayed += pending.len();
        }
        thread::sleep(Duration::from_millis(PREPATCH_POLL_MS));
    }

    if !pending.is_empty() {
        let ready = pending
            .values()
            .map(|entry| entry.block)
            .collect::<Vec<_>>();
        patch_dirty_pages_img(mem, ram_ranges, stage_dir, &ready)?;
        copied += ready.len();
    }
    crate::timing_event(&format!(
        "snapshot.prepatch.stopped copied_blocks={copied} delayed_samples={delayed}"
    ));
    Ok(())
}

struct PendingBlock {
    block: hvf::DirtyBlock,
    redirties: u32,
    due: Instant,
}

impl PendingBlock {
    fn new(block: hvf::DirtyBlock) -> Self {
        Self {
            block,
            redirties: 0,
            due: Instant::now() + Duration::from_millis(PREPATCH_COLD_DELAY_MS),
        }
    }

    fn backoff(&self) -> Duration {
        let shift = self.redirties.min(4);
        let ms = (PREPATCH_COLD_DELAY_MS << shift).min(PREPATCH_MAX_BACKOFF_MS);
        Duration::from_millis(ms)
    }
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
fn pause_vcpus(handles: &[crate::vstate::VcpuHandle], vcpu_ids: &[u64]) -> Result<Vec<Vec<u8>>> {
    use crate::vstate::{VcpuEvent, VcpuResponse};

    for h in handles {
        h.send_event(VcpuEvent::Pause)
            .map_err(|e| SnapshotError::Io(std::io::Error::other(format!("send Pause: {e:?}"))))?;
    }
    // Kick each vCPU so it returns from hv_vcpu_run and picks up the event.
    for &id in vcpu_ids {
        let _ = hvf::vcpu_request_exit(id);
    }

    let mut out = Vec::with_capacity(handles.len());
    for (i, h) in handles.iter().enumerate() {
        match h
            .response_receiver()
            .recv_timeout(std::time::Duration::from_millis(VCPU_PAUSE_TIMEOUT_MS))
        {
            Ok(VcpuResponse::Paused(bytes)) => out.push(bytes),
            Ok(other) => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: unexpected response {other:?}"
                ))));
            }
            Err(RecvTimeoutError::Timeout) => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: pause timeout"
                ))));
            }
            Err(e) => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: {e}"
                ))));
            }
        }
    }
    Ok(out)
}

pub fn resume_vcpus(handles: &[crate::vstate::VcpuHandle]) -> Result<()> {
    use crate::vstate::{VcpuEvent, VcpuResponse};
    for h in handles {
        h.send_event(VcpuEvent::Resume)
            .map_err(|e| SnapshotError::Io(std::io::Error::other(format!("send Resume: {e:?}"))))?;
    }
    for (i, h) in handles.iter().enumerate() {
        match h
            .response_receiver()
            .recv_timeout(std::time::Duration::from_millis(VCPU_PAUSE_TIMEOUT_MS))
        {
            Ok(VcpuResponse::Resumed) => {}
            Ok(other) => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: unexpected resume response {other:?}"
                ))));
            }
            Err(e) => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: resume recv: {e}"
                ))));
            }
        }
    }
    Ok(())
}

/// Restore-side: given a fully-built (post-activate but pre-vCPU-run) VMM and
/// a SnapshotReader, push the captured state into vCPUs, GIC, and devices,
/// then re-arm the virtual timer. Caller has already constructed memory from
/// `pages.img`, so guest RAM is in place.
pub fn restore(inputs: &CaptureInputs<'_>, reader: &super::SnapshotReader) -> Result<()> {
    use crate::vstate::{VcpuEvent, VcpuResponse};

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
    crate::timing_event("snapshot.restore.config.checked");

    let linux_gic = if reader.format == SnapshotFormat::Linux {
        restore_linux_gic_state(reader, inputs.vcpu_handles.len())?
    } else {
        None
    };

    // vCPUs were pre-paused by the builder (queue_initial_pause), so they're
    // already blocked at the top of their first loop iteration. Drain their
    // initial Paused responses before sending RestoreState.
    for (i, h) in inputs.vcpu_handles.iter().enumerate() {
        match h
            .response_receiver()
            .recv_timeout(std::time::Duration::from_millis(VCPU_PAUSE_TIMEOUT_MS))
        {
            Ok(VcpuResponse::Paused(_)) => {}
            Ok(other) => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: expected initial Paused, got {other:?}"
                ))));
            }
            Err(RecvTimeoutError::Timeout) => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: initial-pause timeout"
                ))));
            }
            Err(e) => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: {e}"
                ))));
            }
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
    if reader.format == SnapshotFormat::Macos
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
            h.send_event(VcpuEvent::RestoreGicRedist(redist_regs))
                .map_err(|e| {
                    SnapshotError::Io(std::io::Error::other(format!(
                        "send RestoreGicRedist: {e:?}"
                    )))
                })?;
            match h
                .response_receiver()
                .recv_timeout(std::time::Duration::from_millis(VCPU_PAUSE_TIMEOUT_MS))
            {
                Ok(VcpuResponse::Restored) => {}
                Ok(VcpuResponse::Error(s)) => {
                    return Err(SnapshotError::Io(std::io::Error::other(format!(
                        "vcpu {i}: redist restore: {s}"
                    ))));
                }
                other => {
                    return Err(SnapshotError::Io(std::io::Error::other(format!(
                        "vcpu {i}: unexpected redist restore response {other:?}"
                    ))));
                }
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
        let event = match reader.format {
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
        h.send_event(event).map_err(|e| {
            SnapshotError::Io(std::io::Error::other(format!("send RestoreState: {e:?}")))
        })?;
        match h
            .response_receiver()
            .recv_timeout(std::time::Duration::from_millis(VCPU_PAUSE_TIMEOUT_MS))
        {
            Ok(VcpuResponse::Restored) => {}
            Ok(VcpuResponse::Error(s)) => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: restore: {s}"
                ))));
            }
            other => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: unexpected {other:?}"
                ))));
            }
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

    let timer_delta = match reader.format {
        SnapshotFormat::Macos => cntvct_el0().wrapping_sub(meta.capture_mach_time),
        SnapshotFormat::Linux => 0,
    };
    for (i, h) in inputs.vcpu_handles.iter().enumerate() {
        crate::timing_event(&format!("snapshot.restore.vcpu.timer.begin index={i}"));
        h.send_event(VcpuEvent::RebaseTimer(timer_delta))
            .map_err(|e| {
                SnapshotError::Io(std::io::Error::other(format!("send RebaseTimer: {e:?}")))
            })?;
        match h
            .response_receiver()
            .recv_timeout(std::time::Duration::from_millis(VCPU_PAUSE_TIMEOUT_MS))
        {
            Ok(VcpuResponse::TimerRebased) => {}
            Ok(VcpuResponse::Error(s)) => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: timer rebase: {s}"
                ))));
            }
            other => {
                return Err(SnapshotError::Io(std::io::Error::other(format!(
                    "vcpu {i}: unexpected timer rebase response {other:?}"
                ))));
            }
        }
        crate::timing_event(&format!("snapshot.restore.vcpu.timer.done index={i}"));
    }

    crate::timing_event("snapshot.restore.dirty_tracking.begin");
    enable_dirty_tracking(inputs)?;
    crate::timing_event("snapshot.restore.dirty_tracking.done");

    // Restore virtio devices — match by MMIO base, not by index, so out-of-scope
    // devices in the current ctx (e.g. virtio-balloon) don't shift the mapping.
    let mut restored_transports = Vec::new();
    for i in 0..meta.virtio_bases.len() {
        crate::timing_event(&format!("snapshot.restore.virtio.begin index={i}"));
        let section = read_virtio_section(reader, i as u32)?;
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
            if reader.format == SnapshotFormat::Linux && live_device_name != snapshot_device_name {
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
            if let Some(hvf_reg) = kvm_cpu_sysreg_to_hvf_ich_reg(offset) {
                state.ich_regs.push((hvf_reg, reg.value));
            } else if !kvm_cpu_sysreg_is_icc_apr(offset) {
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
    index: u32,
) -> Result<RestoredVirtioMmioSection> {
    match reader.format {
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

        let path = dir.join("vmstate.bin");
        let mut bytes = std::fs::read(&path).expect("read vmstate");
        bytes[8..12].copy_from_slice(&3u32.to_le_bytes());
        std::fs::write(&path, bytes).expect("rewrite vmstate");

        let reader = super::super::SnapshotReader::open(&dir).expect("open");
        let decoded = read_virtio_section(&reader, 0).expect("decode");
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
}
