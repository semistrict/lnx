use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;
use std::sync::Arc;

use vm_memory::mmap::{GuestRegionMmap, MmapRegionBuilder};
use vm_memory::{
    Address, Bytes, FileOffset, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion,
};

pub use crate::snapshot_metadata::{RamLayout, RamRegion};

use super::{Result, SnapshotError, pages_img_path, snapshot_sync_enabled};

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

/// Build private COW guest RAM backed by `<dir>/pages.img`. Restore is
/// demand-paged: untouched pages cost no host memory, clean pages are shared
/// through the page cache by every VM restoring the same snapshot, and guest
/// writes go to anonymous COW pages without dirtying the file. Falls back to
/// copying into anonymous memory (skipping all-zero chunks, which fresh
/// mappings already read as) when the snapshot lives on a filesystem that
/// cannot back a private mapping.
pub fn restore_pages_img(dir: &Path, layout: &RamLayout) -> Result<GuestMemoryMmap> {
    let pages_path = pages_img_path(dir);
    let file = Arc::new(File::open(&pages_path)?);
    let file_len = file.metadata()?.len();
    for r in &layout.regions {
        let end = r
            .file_offset
            .checked_add(r.size)
            .ok_or(SnapshotError::Truncated)?;
        // Mapping past EOF turns guest RAM accesses into SIGBUS; refuse early.
        if end > file_len {
            return Err(SnapshotError::Truncated);
        }
    }
    if !fs_supports_kvm_file_backing(&file) {
        info!(
            "snapshot.ram.copy_restore reason=fuse_backed path={}",
            pages_path.display()
        );
        return restore_copied(&file, &pages_path, layout);
    }
    match restore_mapped(&file, layout) {
        Ok(memory) => Ok(memory),
        Err(e) => {
            warn!("snapshot.ram.map_failed falling back to copy: {e}");
            restore_copied(&file, &pages_path, layout)
        }
    }
}

/// KVM stage-2 faults cannot pin pages of FUSE-backed private file mappings:
/// the first guest access to such RAM fails KVM_RUN with EFAULT. Snapshots on
/// virtiofs/9p (the nested test setups) must restore by copying instead.
fn fs_supports_kvm_file_backing(file: &File) -> bool {
    use std::os::fd::AsRawFd;

    const FUSE_SUPER_MAGIC: i64 = 0x65735546;
    const V9FS_MAGIC: i64 = 0x0102_1997;

    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut stat) } != 0 {
        return false;
    }
    let fs_type = stat.f_type as i64;
    fs_type != FUSE_SUPER_MAGIC && fs_type != V9FS_MAGIC
}

fn restore_mapped(file: &Arc<File>, layout: &RamLayout) -> Result<GuestMemoryMmap> {
    let mut regions = Vec::new();
    for r in &layout.regions {
        let mapping = MmapRegionBuilder::new(r.size as usize)
            .with_file_offset(FileOffset::from_arc(Arc::clone(file), r.file_offset))
            .with_mmap_prot(libc::PROT_READ | libc::PROT_WRITE)
            .with_mmap_flags(libc::MAP_PRIVATE | libc::MAP_NORESERVE)
            .build()
            .map_err(|e| SnapshotError::Io(std::io::Error::other(format!("{e:?}"))))?;
        let region = GuestRegionMmap::new(mapping, GuestAddress(r.guest_addr))
            .ok_or_else(|| SnapshotError::Io(std::io::Error::other("invalid guest region")))?;
        regions.push(region);
    }
    GuestMemoryMmap::from_regions(regions)
        .map_err(|e| SnapshotError::Io(std::io::Error::other(format!("{e:?}"))))
}

fn restore_copied(file: &File, pages_path: &Path, layout: &RamLayout) -> Result<GuestMemoryMmap> {
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
        let mut copied = 0u64;
        while copied < r.size {
            let size = (r.size - copied).min(buf.len() as u64) as usize;
            file.read_exact_at(&mut buf[..size], r.file_offset + copied)
                .map_err(|e| {
                    SnapshotError::Io(std::io::Error::other(format!(
                        "read {} offset={} len={size}: {e}",
                        pages_path.display(),
                        r.file_offset + copied
                    )))
                })?;
            // Fresh anonymous mappings already read as zeros; writing zero
            // chunks would only force the pages to materialize.
            if buf[..size].iter().any(|byte| *byte != 0) {
                mem.write_slice(&buf[..size], GuestAddress(r.guest_addr + copied))
                    .map_err(|e| {
                        SnapshotError::Io(std::io::Error::other(format!(
                            "write restored RAM 0x{:x}: {e:?}",
                            r.guest_addr + copied
                        )))
                    })?;
            }
            copied += size as u64;
        }
    }
    Ok(mem)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    const MIB: u64 = 1024 * 1024;

    fn pattern(len: usize, salt: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(salt))
            .collect()
    }

    #[test]
    fn pages_img_round_trips_sparsely_and_restores_lazily() {
        let dir = std::env::temp_dir().join(format!("lnx-pages-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let ranges = [(0x8000_0000u64, 4 * MIB), (0x9000_0000u64, 2 * MIB)];
        let mem = GuestMemoryMmap::from_ranges(&[
            (GuestAddress(ranges[0].0), ranges[0].1 as usize),
            (GuestAddress(ranges[1].0), ranges[1].1 as usize),
        ])
        .expect("guest memory");

        // Scatter data with large zero gaps between writes.
        let chunks = [
            (0x8000_0000u64 + 4096, pattern(8192, 1)),
            (0x8000_0000u64 + 3 * MIB, pattern(1234, 2)),
            (0x9000_0000u64 + MIB + 17, pattern(4096, 3)),
        ];
        for (addr, bytes) in &chunks {
            mem.write_slice(bytes, GuestAddress(*addr))
                .expect("write pattern");
        }

        let layout = write_full_pages_img(&mem, &ranges, &dir).expect("capture");
        assert_eq!(layout.regions.len(), 2);
        assert_eq!(layout.regions[1].file_offset, 4 * MIB);

        let meta = std::fs::metadata(pages_img_path(&dir)).expect("stat pages.img");
        assert_eq!(meta.len(), 6 * MIB, "logical size covers all RAM");
        // Capture skips zeros at 1 MiB chunk granularity and the three data
        // chunks land in three distinct chunks, so exactly those materialize.
        assert_eq!(
            meta.blocks() * 512,
            3 * MIB,
            "only chunks containing data may be allocated"
        );

        let restored = restore_pages_img(&dir, &layout).expect("restore");
        for (addr, bytes) in &chunks {
            let mut got = vec![0u8; bytes.len()];
            restored
                .read_slice(&mut got, GuestAddress(*addr))
                .expect("read pattern");
            assert_eq!(&got, bytes, "data at 0x{addr:x} survives restore");
        }
        let mut zeros = vec![0u8; 64 * 1024];
        restored
            .read_slice(&mut zeros, GuestAddress(0x8000_0000 + MIB))
            .expect("read hole");
        assert!(
            zeros.iter().all(|byte| *byte == 0),
            "holes restore as zeros"
        );

        // Guest writes must stay private to the mapping, not dirty the file.
        restored
            .write_slice(&pattern(4096, 9), GuestAddress(0x8000_0000 + 4096))
            .expect("cow write");
        let mut on_disk = vec![0u8; 4096];
        std::fs::File::open(pages_img_path(&dir))
            .expect("open pages.img")
            .read_exact_at(&mut on_disk, 4096)
            .expect("read pages.img");
        assert_eq!(
            &on_disk,
            &pattern(4096, 1)[..4096],
            "file unchanged by guest write"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_refuses_truncated_pages_img() {
        let dir = std::env::temp_dir().join(format!("lnx-pages-trunc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let ranges = [(0x8000_0000u64, 2 * MIB)];
        let mem =
            GuestMemoryMmap::from_ranges(&[(GuestAddress(ranges[0].0), ranges[0].1 as usize)])
                .expect("guest memory");
        let layout = write_full_pages_img(&mem, &ranges, &dir).expect("capture");

        let file = OpenOptions::new()
            .write(true)
            .open(pages_img_path(&dir))
            .expect("open pages.img");
        file.set_len(MIB).expect("truncate");

        assert!(matches!(
            restore_pages_img(&dir, &layout),
            Err(SnapshotError::Truncated)
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
