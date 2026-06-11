use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use devices::legacy::IrqChipDevice;
use devices::virtio::{DeviceSnapshot, MmioTransport, MmioTransportState};
use serde::{Deserialize, Serialize};
use vm_memory::GuestMemoryMmap;

use crate::linux::snapshot::container::{SectionId, SnapshotReader, SnapshotWriter};
use crate::linux::snapshot::ram::{RamLayout, write_full_pages_img};
use crate::linux::snapshot::{Result, SnapshotError};
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
    let mut restored_transports = Vec::new();
    for (i, base) in meta.virtio_bases.iter().enumerate() {
        let section: VirtioMmioSection = reader.get_bincode(SectionId::VirtioMmio, i as u32)?;
        if section.mmio_base != *base {
            return Err(SnapshotError::ConfigMismatch(format!(
                "virtio base mismatch index={i} meta=0x{base:x} section=0x{:x}",
                section.mmio_base
            )));
        }
        let transport_arc = transports.get(base).ok_or_else(|| {
            SnapshotError::ConfigMismatch(format!("no virtio device at base 0x{base:x}"))
        })?;
        restore_virtio_section(transport_arc, &section)?;
        restored_transports.push((section.device_type, transport_arc.clone()));
    }

    if let Some(irqchip) = inputs.irqchip {
        if let Ok(bytes) = reader.get_raw(SectionId::HvfGic, 0) {
            irqchip
                .lock()
                .unwrap()
                .restore_snapshot_state(bytes)
                .map_err(|e| SnapshotError::DeviceRefused(format!("irqchip restore: {e:?}")))?;
        }
    }

    for (index, handle) in inputs.vcpu_handles.iter().enumerate() {
        let bytes = reader.get_raw(SectionId::Vcpu, index as u32)?;
        handle
            .send_event(VcpuEvent::RestoreState(bytes.to_vec()))
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

    let timer_delta = host_timer_counter().wrapping_sub(meta.capture_counter);
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

    post_restore_devices(inputs)?;
    for (device_type, transport) in &restored_transports {
        if device_type == "block" {
            transport.lock().unwrap().replay_queue_notifications();
        }
    }
    for (_, transport) in inputs.virtio_transports {
        transport.lock().unwrap().replay_pending_interrupt();
    }
    Ok(())
}

fn restore_virtio_section(
    transport_arc: &Arc<Mutex<MmioTransport>>,
    section: &VirtioMmioSection,
) -> Result<()> {
    {
        let mut transport = transport_arc.lock().unwrap();
        if let Some(device_snap) = &section.device {
            transport
                .restore_queues_and_activate(&section.transport, &device_snap.queues)
                .map_err(|e| {
                    SnapshotError::DeviceRefused(format!(
                        "base=0x{:x}: activate: {e}",
                        section.mmio_base
                    ))
                })?;
        } else {
            transport.restore_state(&section.transport);
        }
    }
    if let Some(device_snap) = &section.device {
        let transport = transport_arc.lock().unwrap();
        let device_arc = transport.device();
        let mut device = device_arc.lock().unwrap();
        device.pause().map_err(|e| {
            SnapshotError::DeviceRefused(format!("base=0x{:x}: pause: {e}", section.mmio_base))
        })?;
        device.restore_state(device_snap).map_err(|e| {
            SnapshotError::DeviceRefused(format!("base=0x{:x}: restore: {e}", section.mmio_base))
        })?;
        device.resume_after_restore().map_err(|e| {
            SnapshotError::DeviceRefused(format!("base=0x{:x}: resume: {e}", section.mmio_base))
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
