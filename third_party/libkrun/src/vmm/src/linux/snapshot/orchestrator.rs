#[cfg(target_arch = "aarch64")]
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use devices::legacy::IrqChipDevice;
use devices::virtio::{DeviceSnapshot, MmioTransport, MmioTransportState};
use serde::{Deserialize, Serialize};
use vm_memory::GuestMemoryMmap;

use crate::linux::snapshot::container::{
    SectionId, SnapshotFormat, SnapshotReader, SnapshotWriter,
};
use crate::linux::snapshot::ram::{RamLayout, write_full_pages_img};
use crate::linux::snapshot::{Result, SnapshotError};
#[cfg(target_arch = "aarch64")]
use crate::linux::vstate::decode_hvf_vcpu_gic_state;
use crate::linux::vstate::{VcpuEvent, VcpuHandle, VcpuResponse};

pub struct CaptureInputs<'a> {
    pub guest_memory: &'a GuestMemoryMmap,
    pub ram_ranges: &'a [(u64, u64)],
    pub vcpu_handles: &'a [VcpuHandle],
    pub irqchip: Option<&'a Arc<Mutex<IrqChipDevice>>>,
    pub virtio_transports: &'a [(u64, Arc<Mutex<MmioTransport>>)],
    pub nested_enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetaSection {
    pub ram: RamLayout,
    pub virtio_bases: Vec<u64>,
    pub vcpu_count: u32,
    pub nested_enabled: bool,
    pub capture_counter: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VirtioMmioSection {
    pub mmio_base: u64,
    pub device_type: String,
    pub transport: MmioTransportState,
    pub device: Option<DeviceSnapshot>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MacosVirtioMmioSection {
    pub mmio_base: u64,
    pub device_type: u32,
    pub transport: MmioTransportState,
    pub device: Option<DeviceSnapshot>,
}

enum RestoredVirtioMmioSection {
    Linux(VirtioMmioSection),
    Macos(MacosVirtioMmioSection),
}

impl RestoredVirtioMmioSection {
    fn mmio_base(&self) -> u64 {
        match self {
            Self::Linux(section) => section.mmio_base,
            Self::Macos(section) => section.mmio_base,
        }
    }

    fn transport(&self) -> &MmioTransportState {
        match self {
            Self::Linux(section) => &section.transport,
            Self::Macos(section) => &section.transport,
        }
    }

    fn device(&self) -> Option<&DeviceSnapshot> {
        match self {
            Self::Linux(section) => section.device.as_ref(),
            Self::Macos(section) => section.device.as_ref(),
        }
    }
}

pub fn capture(inputs: CaptureInputs<'_>, dir: &Path) -> Result<()> {
    capture_with_paused_hook(inputs, dir, |_| Ok(()))
}

pub fn capture_with_paused_hook<F>(
    inputs: CaptureInputs<'_>,
    dir: &Path,
    paused_hook: F,
) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
    pause_vcpus(&inputs)?;
    let result = capture_paused(&inputs, dir, paused_hook);
    let resume_result = resume_after_capture(&inputs);
    result.and(resume_result)
}

fn capture_paused<F>(inputs: &CaptureInputs<'_>, dir: &Path, paused_hook: F) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let vcpu_states = collect_paused_vcpu_states(inputs)?;
    let virtio_sections = pause_and_snapshot_devices(inputs)?;
    let irqchip_state = match inputs.irqchip {
        Some(irqchip) => irqchip
            .lock()
            .unwrap()
            .snapshot_state()
            .map_err(|e| SnapshotError::DeviceRefused(format!("irqchip snapshot: {e:?}")))?,
        None => None,
    };

    let stage_dir = staging_dir(dir);
    if stage_dir.exists() {
        std::fs::remove_dir_all(&stage_dir)?;
    }
    std::fs::create_dir_all(&stage_dir)?;

    let result = (|| {
        let ram = write_full_pages_img(inputs.guest_memory, inputs.ram_ranges, &stage_dir)?;
        let (total_ram, ram_base) = ram_totals(inputs.ram_ranges);
        let meta = MetaSection {
            ram,
            virtio_bases: virtio_sections.iter().map(|s| s.mmio_base).collect(),
            vcpu_count: inputs.vcpu_handles.len() as u32,
            nested_enabled: inputs.nested_enabled,
            capture_counter: host_timer_counter(),
        };

        let mut writer = SnapshotWriter::new(total_ram, ram_base, meta.vcpu_count);
        writer.add_bincode(SectionId::Meta, 0, &meta)?;
        for (i, bytes) in vcpu_states.iter().enumerate() {
            writer.add_raw(SectionId::Vcpu, i as u32, bytes.clone());
        }
        if let Some(irqchip_state) = irqchip_state {
            writer.add_raw(SectionId::HvfGic, 0, irqchip_state);
        }
        for (i, section) in virtio_sections.iter().enumerate() {
            writer.add_bincode(SectionId::VirtioMmio, i as u32, section)?;
        }
        writer.write_to_dir(&stage_dir)?;
        paused_hook(&stage_dir)?;
        publish_snapshot_dir(&stage_dir, dir)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_dir_all(&stage_dir);
    }
    result
}

fn collect_paused_vcpu_states(inputs: &CaptureInputs<'_>) -> Result<Vec<Vec<u8>>> {
    let mut states = Vec::with_capacity(inputs.vcpu_handles.len());
    for (index, handle) in inputs.vcpu_handles.iter().enumerate() {
        match handle
            .response_receiver()
            .recv()
            .map_err(|e| SnapshotError::DeviceRefused(format!("vcpu {index}: {e}")))?
        {
            VcpuResponse::Paused(bytes) => states.push(bytes),
            VcpuResponse::Error(e) => {
                return Err(SnapshotError::DeviceRefused(format!("vcpu {index}: {e}")));
            }
            other => {
                return Err(SnapshotError::DeviceRefused(format!(
                    "vcpu {index}: unexpected response {other:?}"
                )));
            }
        }
    }
    Ok(states)
}

fn pause_vcpus(inputs: &CaptureInputs<'_>) -> Result<()> {
    for (index, handle) in inputs.vcpu_handles.iter().enumerate() {
        handle
            .send_event(VcpuEvent::Pause)
            .map_err(|e| SnapshotError::DeviceRefused(format!("pause vcpu {index}: {e:?}")))?;
    }
    Ok(())
}

fn resume_after_capture(inputs: &CaptureInputs<'_>) -> Result<()> {
    resume_devices(inputs)?;
    for (index, handle) in inputs.vcpu_handles.iter().enumerate() {
        handle
            .send_event(VcpuEvent::Resume)
            .map_err(|e| SnapshotError::DeviceRefused(format!("resume vcpu {index}: {e:?}")))?;
    }
    for (index, handle) in inputs.vcpu_handles.iter().enumerate() {
        match handle
            .response_receiver()
            .recv()
            .map_err(|e| SnapshotError::DeviceRefused(format!("resume vcpu {index}: {e}")))?
        {
            VcpuResponse::Resumed => {}
            VcpuResponse::Error(e) => {
                return Err(SnapshotError::DeviceRefused(format!(
                    "resume vcpu {index}: {e}"
                )));
            }
            other => {
                return Err(SnapshotError::DeviceRefused(format!(
                    "resume vcpu {index}: unexpected response {other:?}"
                )));
            }
        }
    }
    Ok(())
}

fn pause_and_snapshot_devices(inputs: &CaptureInputs<'_>) -> Result<Vec<VirtioMmioSection>> {
    let mut sections = Vec::new();
    for (base, transport_arc) in inputs.virtio_transports {
        let transport = transport_arc.lock().unwrap();
        let device_arc = transport.device();
        let mut device = device_arc.lock().unwrap();
        let device_type = device.device_name().to_string();
        device
            .pause()
            .map_err(|e| SnapshotError::DeviceRefused(format!("base=0x{base:x}: pause: {e}")))?;
        let device_snap = match device.serialize_state() {
            Ok(snap) => Some(snap),
            Err(e) => {
                let _ = device.resume();
                return Err(SnapshotError::DeviceRefused(format!(
                    "base=0x{base:x}: {e}"
                )));
            }
        };
        let transport_state = transport.to_state();
        drop(device);
        drop(transport);
        sections.push(VirtioMmioSection {
            mmio_base: *base,
            device_type,
            transport: transport_state,
            device: device_snap,
        });
    }
    Ok(sections)
}

fn resume_devices(inputs: &CaptureInputs<'_>) -> Result<()> {
    for (base, transport_arc) in inputs.virtio_transports {
        let transport = transport_arc.lock().unwrap();
        let device_arc = transport.device();
        let mut device = device_arc.lock().unwrap();
        device
            .resume()
            .map_err(|e| SnapshotError::DeviceRefused(format!("base=0x{base:x}: resume: {e}")))?;
    }
    Ok(())
}

pub fn restore(inputs: &CaptureInputs<'_>, reader: &SnapshotReader) -> Result<()> {
    let meta: MetaSection = reader.get_bincode(SectionId::Meta, 0)?;
    if meta.vcpu_count != inputs.vcpu_handles.len() as u32 {
        return Err(SnapshotError::ConfigMismatch(format!(
            "vcpu count snapshot={} current={}",
            meta.vcpu_count,
            inputs.vcpu_handles.len()
        )));
    }

    let transports = inputs
        .virtio_transports
        .iter()
        .map(|(base, transport)| (*base, transport.clone()))
        .collect::<std::collections::HashMap<_, _>>();
    let mut pending_virtio_sections = Vec::new();
    for (i, base) in meta.virtio_bases.iter().enumerate() {
        let section = read_virtio_section(reader, i as u32)?;
        if section.mmio_base() != *base {
            return Err(SnapshotError::ConfigMismatch(format!(
                "virtio base mismatch index={i} meta=0x{base:x} section=0x{:x}",
                section.mmio_base()
            )));
        }
        let transport_arc = transports.get(base).ok_or_else(|| {
            SnapshotError::ConfigMismatch(format!("no virtio device at base 0x{base:x}"))
        })?;
        pending_virtio_sections.push((i, section, transport_arc.clone()));
    }

    if reader.format == SnapshotFormat::Linux {
        if let Some(irqchip) = inputs.irqchip {
            if let Ok(bytes) = reader.get_raw(SectionId::HvfGic, 0) {
                irqchip
                    .lock()
                    .unwrap()
                    .restore_snapshot_state(bytes)
                    .map_err(|e| SnapshotError::DeviceRefused(format!("irqchip restore: {e:?}")))?;
            }
        }
    } else {
        restore_macos_irqchip_state(inputs, reader, meta.capture_counter)?;
        restore_macos_virtio_irq_types(inputs, &pending_virtio_sections)?;
    }

    for (index, handle) in inputs.vcpu_handles.iter().enumerate() {
        let bytes = reader.get_raw(SectionId::Vcpu, index as u32)?;
        let event = match reader.format {
            SnapshotFormat::Linux => VcpuEvent::RestoreState(bytes.to_vec()),
            SnapshotFormat::Macos => VcpuEvent::RestoreHvfState {
                state: bytes.to_vec(),
                capture_counter: meta.capture_counter,
            },
        };
        handle
            .send_event(event)
            .map_err(|e| SnapshotError::DeviceRefused(format!("restore vcpu {index}: {e:?}")))?;
    }
    for (index, handle) in inputs.vcpu_handles.iter().enumerate() {
        match handle
            .response_receiver()
            .recv()
            .map_err(|e| SnapshotError::DeviceRefused(format!("restore vcpu {index}: {e}")))?
        {
            VcpuResponse::Restored => {}
            VcpuResponse::Error(e) => {
                return Err(SnapshotError::DeviceRefused(format!(
                    "restore vcpu {index}: {e}"
                )));
            }
            other => {
                return Err(SnapshotError::DeviceRefused(format!(
                    "restore vcpu {index}: unexpected response {other:?}"
                )));
            }
        }
    }

    let timer_delta = restore_timer_delta(reader.format, meta.capture_counter);
    for (index, handle) in inputs.vcpu_handles.iter().enumerate() {
        handle
            .send_event(VcpuEvent::RebaseTimer(timer_delta))
            .map_err(|e| SnapshotError::DeviceRefused(format!("rebase vcpu {index}: {e:?}")))?;
    }
    for (index, handle) in inputs.vcpu_handles.iter().enumerate() {
        match handle
            .response_receiver()
            .recv()
            .map_err(|e| SnapshotError::DeviceRefused(format!("rebase vcpu {index}: {e}")))?
        {
            VcpuResponse::TimerRebased => {}
            VcpuResponse::Error(e) => {
                return Err(SnapshotError::DeviceRefused(format!(
                    "rebase vcpu {index}: {e}"
                )));
            }
            other => {
                return Err(SnapshotError::DeviceRefused(format!(
                    "rebase vcpu {index}: unexpected response {other:?}"
                )));
            }
        }
    }

    let mut restored_transports = Vec::new();
    for (_, section, transport_arc) in pending_virtio_sections {
        restore_virtio_section(&transport_arc, &section)?;
        let device_name = {
            let transport = transport_arc.lock().unwrap();
            transport.locked_device().device_name().to_string()
        };
        restored_transports.push((device_name, transport_arc));
    }

    post_restore_devices(inputs)?;
    for (device_type, transport) in &restored_transports {
        if matches!(device_type.as_str(), "block" | "vsock") {
            transport.lock().unwrap().replay_queue_notifications();
        }
    }
    for (_, transport) in inputs.virtio_transports {
        let transport = transport.lock().unwrap();
        transport.replay_pending_interrupt();
    }
    Ok(())
}

#[cfg(target_arch = "aarch64")]
fn restore_macos_irqchip_state(
    inputs: &CaptureInputs<'_>,
    reader: &SnapshotReader,
    _capture_counter: u64,
) -> Result<()> {
    let Some(irqchip) = inputs.irqchip else {
        return Ok(());
    };
    let dist_bytes = reader.get_raw(SectionId::GicDist, 0).unwrap_or(&[]);
    irqchip
        .lock()
        .unwrap()
        .restore_macos_gic_dist_state(dist_bytes)
        .map_err(|e| SnapshotError::DeviceRefused(format!("macOS irqchip restore: {e:?}")))?;
    for index in 0..inputs.vcpu_handles.len() {
        let bytes = reader.get_raw(SectionId::Vcpu, index as u32)?;
        let (icc_regs, redist_regs) = decode_hvf_vcpu_gic_state(bytes)?;
        irqchip
            .lock()
            .unwrap()
            .restore_macos_vcpu_gic_state(index as u64, &icc_regs, &redist_regs)
            .map_err(|e| {
                SnapshotError::DeviceRefused(format!("macOS vcpu {index} irqchip restore: {e:?}"))
            })?;
    }
    Ok(())
}

#[cfg(not(target_arch = "aarch64"))]
fn restore_macos_irqchip_state(
    _inputs: &CaptureInputs<'_>,
    _reader: &SnapshotReader,
    _capture_counter: u64,
) -> Result<()> {
    Ok(())
}

#[cfg(target_arch = "aarch64")]
fn restore_macos_virtio_irq_types(
    inputs: &CaptureInputs<'_>,
    pending_virtio_sections: &[(usize, RestoredVirtioMmioSection, Arc<Mutex<MmioTransport>>)],
) -> Result<()> {
    let Some(irqchip) = inputs.irqchip else {
        return Ok(());
    };

    let mut irqs = BTreeSet::new();
    for (_, section, transport_arc) in pending_virtio_sections {
        let live_irq = transport_arc.lock().unwrap().to_state().irq_line;
        if let Some(irq) = live_irq.or(section.transport().irq_line) {
            irqs.insert(irq);
        }
    }

    if irqs.is_empty() {
        return Ok(());
    }

    let irqs = irqs.into_iter().collect::<Vec<_>>();
    irqchip
        .lock()
        .unwrap()
        .restore_macos_virtio_edge_irqs(&irqs)
        .map_err(|e| SnapshotError::DeviceRefused(format!("macOS virtio irq type restore: {e:?}")))
}

#[cfg(not(target_arch = "aarch64"))]
fn restore_macos_virtio_irq_types(
    _inputs: &CaptureInputs<'_>,
    _pending_virtio_sections: &[(usize, RestoredVirtioMmioSection, Arc<Mutex<MmioTransport>>)],
) -> Result<()> {
    Ok(())
}

fn restore_timer_delta(format: SnapshotFormat, capture_counter: u64) -> u64 {
    match format {
        SnapshotFormat::Linux => host_timer_counter().wrapping_sub(capture_counter),
        SnapshotFormat::Macos => {
            crate::timing_event("snapshot.restore.macos.skip_timer_rebase");
            0
        }
    }
}

fn read_virtio_section(reader: &SnapshotReader, index: u32) -> Result<RestoredVirtioMmioSection> {
    match reader.format {
        SnapshotFormat::Linux => reader
            .get_bincode(SectionId::VirtioMmio, index)
            .map(RestoredVirtioMmioSection::Linux),
        SnapshotFormat::Macos => reader
            .get_bincode(SectionId::VirtioMmio, index)
            .map(RestoredVirtioMmioSection::Macos),
    }
}

fn restore_virtio_section(
    transport_arc: &Arc<Mutex<MmioTransport>>,
    section: &RestoredVirtioMmioSection,
) -> Result<()> {
    let mmio_base = section.mmio_base();
    {
        let mut transport = transport_arc.lock().unwrap();
        if let Some(device_snap) = section.device() {
            transport
                .restore_queues_and_activate(section.transport(), &device_snap.queues)
                .map_err(|e| {
                    SnapshotError::DeviceRefused(format!("base=0x{mmio_base:x}: activate: {e}"))
                })?;
        } else {
            transport.restore_state(section.transport());
        }
    }
    if let Some(device_snap) = section.device() {
        let transport = transport_arc.lock().unwrap();
        let device_arc = transport.device();
        let mut device = device_arc.lock().unwrap();
        device.pause().map_err(|e| {
            SnapshotError::DeviceRefused(format!("base=0x{mmio_base:x}: pause: {e}"))
        })?;
        let restore_result = match section {
            RestoredVirtioMmioSection::Linux(_) => device.restore_state(device_snap),
            RestoredVirtioMmioSection::Macos(_) => device.restore_macos_state(device_snap),
        };
        restore_result.map_err(|e| {
            SnapshotError::DeviceRefused(format!("base=0x{mmio_base:x}: restore: {e}"))
        })?;
        device.resume_after_restore().map_err(|e| {
            SnapshotError::DeviceRefused(format!("base=0x{mmio_base:x}: resume: {e}"))
        })?;
    }
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

#[cfg(target_arch = "aarch64")]
fn host_timer_counter() -> u64 {
    let value: u64;
    unsafe {
        std::arch::asm!("mrs {value}, cntvct_el0", value = out(reg) value);
    }
    value
}

#[cfg(not(target_arch = "aarch64"))]
fn host_timer_counter() -> u64 {
    0
}

fn staging_dir(dir: &Path) -> PathBuf {
    let mut name = dir
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| ".snapshot".into());
    name.push(".staging");
    dir.with_file_name(name)
}

fn publish_snapshot_dir(stage_dir: &Path, dir: &Path) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::rename(stage_dir, dir)?;
    Ok(())
}

fn ram_totals(ranges: &[(u64, u64)]) -> (u64, u64) {
    let mut total_ram = 0u64;
    let mut ram_base = u64::MAX;
    for (addr, size) in ranges {
        total_ram += *size;
        ram_base = ram_base.min(*addr);
    }
    (total_ram, ram_base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::snapshot::vmstate_path;

    fn transport_state() -> MmioTransportState {
        MmioTransportState {
            features_select: 1,
            acked_features_select: 2,
            queue_select: 3,
            device_status: 4,
            config_generation: 5,
            shm_region_select: 6,
            interrupt_status: 7,
            irq_line: Some(8),
        }
    }

    #[test]
    fn reads_linux_virtio_section_with_string_device_type() {
        let dir =
            std::env::temp_dir().join(format!("lnx-linux-virtio-section-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let section = VirtioMmioSection {
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

        let reader = SnapshotReader::open(&dir).expect("open");
        let decoded = read_virtio_section(&reader, 0).expect("decode");
        match decoded {
            RestoredVirtioMmioSection::Linux(decoded) => {
                assert_eq!(decoded.mmio_base, section.mmio_base);
                assert_eq!(decoded.device_type, "block");
                assert_eq!(
                    decoded.transport.device_status,
                    section.transport.device_status
                );
            }
            RestoredVirtioMmioSection::Macos(_) => panic!("expected linux section"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_macos_virtio_section_with_numeric_device_type() {
        let dir =
            std::env::temp_dir().join(format!("lnx-macos-virtio-section-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let section = MacosVirtioMmioSection {
            mmio_base: 0x1000_0000,
            device_type: 2,
            transport: transport_state(),
            device: None,
        };
        let mut writer = SnapshotWriter::new(0x4000_0000, 0x8000_0000, 1);
        writer
            .add_bincode(SectionId::VirtioMmio, 0, &section)
            .expect("add virtio");
        writer.write_to_dir(&dir).expect("write");

        let path = vmstate_path(&dir);
        let mut bytes = std::fs::read(&path).expect("read vmstate");
        bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
        std::fs::write(&path, bytes).expect("rewrite vmstate");

        let reader = SnapshotReader::open(&dir).expect("open");
        let decoded = read_virtio_section(&reader, 0).expect("decode");
        match decoded {
            RestoredVirtioMmioSection::Macos(decoded) => {
                assert_eq!(decoded.mmio_base, section.mmio_base);
                assert_eq!(decoded.device_type, 2);
                assert_eq!(
                    decoded.transport.device_status,
                    section.transport.device_status
                );
            }
            RestoredVirtioMmioSection::Linux(_) => panic!("expected macos section"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn macos_restore_timer_delta_is_zero() {
        assert_eq!(restore_timer_delta(SnapshotFormat::Macos, u64::MAX), 0);
    }
}
