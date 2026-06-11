use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;

use serde::{Deserialize, Serialize};
use vm_memory::mmap::{GuestRegionMmap, MmapRegionBuilder};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

use super::{Result, SnapshotError, pages_img_path, snapshot_sync_enabled};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RamRegion {
    pub guest_addr: u64,
    pub size: u64,
    pub file_offset: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RamLayout {
    pub regions: Vec<RamRegion>,
}

pub fn write_full_pages_img(
    mem: &GuestMemoryMmap,
    ram_ranges: &[(u64, u64)],
    dir: &Path,
) -> Result<RamLayout> {
    let layout = layout_from_ranges(ram_ranges);
    let path = pages_img_path(dir);
    // Guest RAM contents can embed secrets; keep the image owner-only.
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    let len = layout
        .regions
        .iter()
        .map(|region| region.file_offset + region.size)
        .max()
        .unwrap_or(0);
    file.set_len(len)?;

    let mut buf = vec![0u8; 1024 * 1024];
    for region in mem
        .iter()
        .filter(|region| is_ram_region(ram_ranges, region.start_addr().raw_value(), region.len()))
    {
        let Some(base_offset) = guest_addr_to_file_offset(&layout, region.start_addr().raw_value())
        else {
            continue;
        };
        let mut copied = 0u64;
        while copied < region.len() {
            let size = (region.len() - copied).min(buf.len() as u64) as usize;
            mem.read_slice(&mut buf[..size], region.start_addr().unchecked_add(copied))
                .map_err(|e| {
                    SnapshotError::Io(std::io::Error::other(format!(
                        "read RAM 0x{:x}: {e:?}",
                        region.start_addr().raw_value() + copied
                    )))
                })?;
            // Zero pages stay holes; the file is pre-sized, so they read back
            // as zeros on restore.
            if buf[..size].iter().any(|byte| *byte != 0) {
                file.write_all_at(&buf[..size], base_offset + copied)?;
            }
            copied += size as u64;
        }
    }
    if snapshot_sync_enabled() {
        file.sync_all()?;
    }
    Ok(layout)
}

fn layout_from_ranges(ranges: &[(u64, u64)]) -> RamLayout {
    let mut layout = RamLayout {
        regions: Vec::new(),
    };
    let mut cursor = 0u64;
    for (start, size) in ranges {
        layout.regions.push(RamRegion {
            guest_addr: *start,
            size: *size,
            file_offset: cursor,
        });
        cursor += *size;
    }
    layout
}

fn is_ram_region(ranges: &[(u64, u64)], guest_addr: u64, size: u64) -> bool {
    ranges
        .iter()
        .any(|(range_addr, range_size)| *range_addr == guest_addr && *range_size == size)
}

fn guest_addr_to_file_offset(layout: &RamLayout, guest_addr: u64) -> Option<u64> {
    for region in &layout.regions {
        if guest_addr >= region.guest_addr && guest_addr < region.guest_addr + region.size {
            return Some(region.file_offset + (guest_addr - region.guest_addr));
        }
    }
    None
}

pub fn restore_pages_img(dir: &Path, layout: &RamLayout) -> Result<GuestMemoryMmap> {
    let pages_path = pages_img_path(dir);
    let mut file = File::open(&pages_path)?;
    let mut regions = Vec::new();
    for r in &layout.regions {
        let mapping = MmapRegionBuilder::new(r.size as usize)
            .with_mmap_prot(libc::PROT_READ | libc::PROT_WRITE)
            .with_mmap_flags(libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE)
            .build()
            .map_err(|e| SnapshotError::Io(std::io::Error::other(format!("{e:?}"))))?;
        let region = GuestRegionMmap::new(mapping, GuestAddress(r.guest_addr))
            .ok_or_else(|| SnapshotError::Io(std::io::Error::other("invalid guest region")))?;
        regions.push(region);
    }
    let mem = GuestMemoryMmap::from_regions(regions)
        .map_err(|e| SnapshotError::Io(std::io::Error::other(format!("{e:?}"))))?;
    let mut buf = vec![0u8; 1024 * 1024];
    for r in &layout.regions {
        file.seek(SeekFrom::Start(r.file_offset))?;
        let mut copied = 0u64;
        while copied < r.size {
            let size = (r.size - copied).min(buf.len() as u64) as usize;
            file.read_exact(&mut buf[..size]).map_err(|e| {
                SnapshotError::Io(std::io::Error::other(format!(
                    "read {} offset={} len={size}: {e}",
                    pages_path.display(),
                    r.file_offset + copied
                )))
            })?;
            mem.write_slice(&buf[..size], GuestAddress(r.guest_addr + copied))
                .map_err(|e| {
                    SnapshotError::Io(std::io::Error::other(format!(
                        "write restored RAM 0x{:x}: {e:?}",
                        r.guest_addr + copied
                    )))
                })?;
            copied += size as u64;
        }
    }
    Ok(mem)
}
