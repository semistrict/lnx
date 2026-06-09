// RAM capture and restore helpers.
//
// Capture: the first snapshot writes guest memory into pages.img. Later
// snapshots clone the previous pages.img and patch only dirty RAM blocks. The
// offsets and region descriptors are recorded in vmstate so restore can rebuild
// the same mapping.
//
// Restore: map pages.img with MAP_PRIVATE so RAM is demand-paged and guest
// writes go to private COW pages. Capture later patches only dirty blocks back
// into the next cloned pages.img.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

use hvf::DirtyBlock;
use serde::{Deserialize, Serialize};
use vm_memory::mmap::{GuestRegionMmap, MmapRegionBuilder};
use vm_memory::{
    Address, Bytes, FileOffset, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion,
};

use super::{Result, SnapshotError, pages_img_path, snapshot_sync_enabled};

/// Description of one guest memory region as preserved in the snapshot.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RamRegion {
    pub guest_addr: u64,
    pub size: u64,
    /// Offset within `pages.img`.
    pub file_offset: u64,
}

/// Snapshot of the guest's memory layout. The actual page contents live in
/// the sibling pages.img file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RamLayout {
    pub regions: Vec<RamRegion>,
}

pub fn write_full_pages_img(
    mem: &GuestMemoryMmap,
    ram_ranges: &[(u64, u64)],
    dir: &Path,
) -> Result<RamLayout> {
    crate::timing_event("snapshot.ram.write_full.begin");
    let layout = layout_from_ranges(ram_ranges);
    let path = pages_img_path(dir);
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)?;
    let len = layout
        .regions
        .iter()
        .map(|region| region.file_offset + region.size)
        .max()
        .unwrap_or(0);
    file.set_len(len)?;
    crate::timing_event("snapshot.ram.write_full.sized");

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
        let mut next_progress = 128 * 1024 * 1024u64;
        while copied < region.len() {
            let size = (region.len() - copied).min(buf.len() as u64) as usize;
            mem.read_slice(&mut buf[..size], region.start_addr().unchecked_add(copied))
                .map_err(|e| {
                    SnapshotError::Io(std::io::Error::other(format!(
                        "read RAM 0x{:x}: {e:?}",
                        region.start_addr().raw_value() + copied
                    )))
                })?;
            let file_offset = base_offset + copied;
            if buf[..size].iter().any(|&byte| byte != 0) {
                file.write_all_at(&buf[..size], file_offset)?;
            } else {
                punch_hole(&file, file_offset, size as u64)?;
            }
            copied += size as u64;
            if copied >= next_progress {
                crate::timing_event(&format!(
                    "snapshot.ram.write_full.progress guest=0x{:x} copied={copied}",
                    region.start_addr().raw_value()
                ));
                next_progress += 128 * 1024 * 1024u64;
            }
        }
    }
    finish_snapshot_file(
        file,
        "snapshot.ram.write_full.synced",
        "snapshot.ram.write_full.sync.skipped",
        "snapshot.ram.write_full.close.deferred",
    )?;
    Ok(layout)
}

#[cfg(target_os = "macos")]
fn punch_hole(file: &File, offset: u64, len: u64) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let mut hole = libc::fpunchhole_t {
        fp_flags: 0,
        reserved: 0,
        fp_offset: offset as libc::off_t,
        fp_length: len as libc::off_t,
    };
    let ret = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &mut hole as *mut _) };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
fn punch_hole(_file: &File, _offset: u64, _len: u64) -> std::io::Result<()> {
    Ok(())
}

pub fn clone_and_patch_dirty_pages_img(
    mem: &GuestMemoryMmap,
    ram_ranges: &[(u64, u64)],
    previous_dir: &Path,
    stage_dir: &Path,
    dirty_blocks: &[DirtyBlock],
) -> Result<RamLayout> {
    crate::timing_event("snapshot.ram.clone_patch.begin");
    let previous_pages = pages_img_path(previous_dir);
    let stage_pages = pages_img_path(stage_dir);
    crate::timing_event("snapshot.ram.clone_pages.begin");
    clone_pages_img(&previous_pages, &stage_pages)?;
    crate::timing_event("snapshot.ram.clone_pages.done");

    patch_dirty_pages_img(mem, ram_ranges, stage_dir, dirty_blocks)
}

pub fn clone_pages_image(src_dir: &Path, dst_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dst_dir)?;
    clone_pages_img(&pages_img_path(src_dir), &pages_img_path(dst_dir))?;
    Ok(())
}

pub fn patch_dirty_pages_img(
    mem: &GuestMemoryMmap,
    ram_ranges: &[(u64, u64)],
    dir: &Path,
    dirty_blocks: &[DirtyBlock],
) -> Result<RamLayout> {
    crate::timing_event("snapshot.ram.patch_existing.begin");
    let layout = layout_from_ranges(ram_ranges);
    let stage_pages = pages_img_path(dir);
    let file = OpenOptions::new().write(true).open(&stage_pages)?;
    crate::timing_event(&format!(
        "snapshot.ram.patch_dirty.begin count={}",
        dirty_blocks.len()
    ));
    patch_dirty_blocks(mem, &file, &layout, dirty_blocks)?;
    crate::timing_event("snapshot.ram.patch_dirty.done");
    finish_snapshot_file(
        file,
        "snapshot.ram.clone_patch.synced",
        "snapshot.ram.clone_patch.sync.skipped",
        "snapshot.ram.clone_patch.close.deferred",
    )?;
    crate::timing_event("snapshot.ram.patch_existing.done");
    Ok(layout)
}

fn finish_snapshot_file(
    file: File,
    synced_event: &str,
    skipped_event: &str,
    deferred_close_event: &str,
) -> Result<()> {
    if snapshot_sync_enabled() {
        file.sync_all()?;
        crate::timing_event(synced_event);
        drop(file);
    } else {
        crate::timing_event(skipped_event);
        let _ = std::thread::spawn(move || drop(file));
        crate::timing_event(deferred_close_event);
    }
    Ok(())
}

fn patch_dirty_blocks(
    mem: &GuestMemoryMmap,
    file: &File,
    layout: &RamLayout,
    dirty_blocks: &[DirtyBlock],
) -> Result<()> {
    let workers = std::thread::available_parallelism()
        .map(|workers| workers.get())
        .unwrap_or(1)
        .min(4)
        .min(dirty_blocks.len());
    if workers <= 1 || dirty_blocks.len() < 8 {
        return patch_dirty_blocks_serial(mem, file, layout, dirty_blocks);
    }

    let chunk_size = dirty_blocks.len().div_ceil(workers);
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for chunk in dirty_blocks.chunks(chunk_size) {
            handles.push(scope.spawn(move || patch_dirty_blocks_serial(mem, file, layout, chunk)));
        }
        for handle in handles {
            match handle.join() {
                Ok(result) => result?,
                Err(_) => {
                    return Err(SnapshotError::Io(std::io::Error::other(
                        "dirty RAM patch thread panicked",
                    )));
                }
            }
        }
        Ok(())
    })
}

fn patch_dirty_blocks_serial(
    mem: &GuestMemoryMmap,
    file: &File,
    layout: &RamLayout,
    dirty_blocks: &[DirtyBlock],
) -> Result<()> {
    let mut buf = vec![0u8; hvf::DIRTY_BLOCK_SIZE as usize];
    for (index, block) in dirty_blocks.iter().enumerate() {
        let Some(file_offset) = guest_addr_to_file_offset(&layout, block.guest_addr) else {
            continue;
        };
        let size = block.size as usize;
        mem.read_slice(&mut buf[..size], GuestAddress(block.guest_addr))
            .map_err(|e| {
                SnapshotError::Io(std::io::Error::other(format!(
                    "read dirty block 0x{:x}: {e:?}",
                    block.guest_addr
                )))
            })?;
        file.write_all_at(&buf[..size], file_offset)?;
        if index > 0 && index % 1024 == 0 {
            crate::timing_event(&format!("snapshot.ram.patch_dirty.progress blocks={index}"));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn clone_pages_img(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let src = CString::new(src.as_os_str().as_bytes())?;
    let dst = CString::new(dst.as_os_str().as_bytes())?;
    let ret = unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), 0) };
    if ret == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
fn clone_pages_img(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::copy(src, dst).map(|_| ())
}

fn layout_from_ranges(ranges: &[(u64, u64)]) -> RamLayout {
    let mut layout = RamLayout {
        regions: Vec::new(),
    };
    let mut cursor: u64 = 0;
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

/// Build private COW guest RAM backed by `<dir>/pages.img`.
pub fn restore_pages_img(dir: &Path, layout: &RamLayout) -> Result<GuestMemoryMmap> {
    crate::timing_event("snapshot.ram.restore_pages_img.begin");
    let file = Arc::new(File::open(pages_img_path(dir))?);
    crate::timing_event("snapshot.ram.pages_img.opened");
    let mut regions = Vec::new();
    for (index, r) in layout.regions.iter().enumerate() {
        let mapping = MmapRegionBuilder::new(r.size as usize)
            .with_file_offset(FileOffset::from_arc(file.clone(), r.file_offset))
            .with_mmap_prot(libc::PROT_READ | libc::PROT_WRITE)
            .with_mmap_flags(libc::MAP_PRIVATE | libc::MAP_NORESERVE)
            .build()
            .map_err(|e| SnapshotError::Io(std::io::Error::other(format!("{e:?}"))))?;
        let region = GuestRegionMmap::new(mapping, GuestAddress(r.guest_addr))
            .ok_or_else(|| SnapshotError::Io(std::io::Error::other("invalid guest region")))?;
        regions.push(region);
        crate::timing_event(&format!(
            "snapshot.ram.region.mapped index={index} size={}",
            r.size
        ));
    }
    let memory = GuestMemoryMmap::from_regions(regions)
        .map_err(|e| SnapshotError::Io(std::io::Error::other(format!("{e:?}"))))?;
    crate::timing_event("snapshot.ram.restore_pages_img.done");
    Ok(memory)
}
