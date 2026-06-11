use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use sha2::{Digest, Sha256};

use super::{Result, SnapshotError, snapshot_sync_enabled, vmstate_path};

const MAGIC: [u8; 8] = *b"LKRNSS01";
// Version history (Linux container; the macOS container versions separately):
//   1: initial full-RAM capture
//   2: virtio-fs sections carry the FUSE server state; v1 snapshots restore
//      to a server with an empty inode table and must not be accepted
// Bump this whenever a section payload changes shape so stale snapshots are
// skipped at the pre-flight header check instead of failing mid-restore.
// Keep in sync with SNAPSHOT_VMSTATE_VERSION in lnx's src/runner.rs.
const VERSION: u32 = 2;
const HEADER_LEN: usize = 40;
const TOC_ENTRY_LEN: usize = 56;

// Some ids are reserved for sections that are not emitted yet; keep them so
// the id space stays stable across versions.
#[allow(dead_code)]
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SectionId {
    Meta = 1,
    Vcpu = 2,
    GicDist = 3,
    GicVcpu = 4,
    VirtioMmio = 5,
    HvfGic = 6,
}

#[derive(Clone, Debug)]
pub struct Header {
    pub version: u32,
    pub ram_size: u64,
    pub ram_base: u64,
    pub vcpu_count: u32,
}

impl Header {
    fn encode(&self, num_sections: u32) -> [u8; HEADER_LEN] {
        let mut buf = [0u8; HEADER_LEN];
        buf[0..8].copy_from_slice(&MAGIC);
        buf[8..12].copy_from_slice(&self.version.to_le_bytes());
        buf[12..16].copy_from_slice(&num_sections.to_le_bytes());
        buf[16..24].copy_from_slice(&self.ram_size.to_le_bytes());
        buf[24..32].copy_from_slice(&self.ram_base.to_le_bytes());
        buf[32..36].copy_from_slice(&self.vcpu_count.to_le_bytes());
        buf
    }

    fn decode(buf: &[u8]) -> Result<(Self, u32)> {
        if buf.len() < HEADER_LEN {
            return Err(SnapshotError::Truncated);
        }
        if buf[0..8] != MAGIC {
            return Err(SnapshotError::BadMagic);
        }
        let version = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        if version != VERSION {
            return Err(SnapshotError::BadVersion(version));
        }
        let num_sections = u32::from_le_bytes(buf[12..16].try_into().unwrap());
        let ram_size = u64::from_le_bytes(buf[16..24].try_into().unwrap());
        let ram_base = u64::from_le_bytes(buf[24..32].try_into().unwrap());
        let vcpu_count = u32::from_le_bytes(buf[32..36].try_into().unwrap());
        Ok((
            Header {
                version,
                ram_size,
                ram_base,
                vcpu_count,
            },
            num_sections,
        ))
    }
}

#[derive(Clone, Debug)]
struct TocEntry {
    id: u32,
    index: u32,
    offset: u64,
    len: u64,
    sha256: [u8; 32],
}

impl TocEntry {
    fn encode(&self, buf: &mut [u8]) {
        buf[0..4].copy_from_slice(&self.id.to_le_bytes());
        buf[4..8].copy_from_slice(&self.index.to_le_bytes());
        buf[8..16].copy_from_slice(&self.offset.to_le_bytes());
        buf[16..24].copy_from_slice(&self.len.to_le_bytes());
        buf[24..56].copy_from_slice(&self.sha256);
    }

    fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < TOC_ENTRY_LEN {
            return Err(SnapshotError::Truncated);
        }
        let id = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let index = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        let offset = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        let len = u64::from_le_bytes(buf[16..24].try_into().unwrap());
        let mut sha256 = [0u8; 32];
        sha256.copy_from_slice(&buf[24..56]);
        Ok(Self {
            id,
            index,
            offset,
            len,
            sha256,
        })
    }
}

pub struct SnapshotWriter {
    header: Header,
    sections: Vec<(u32, u32, Vec<u8>)>,
}

impl SnapshotWriter {
    pub fn new(ram_size: u64, ram_base: u64, vcpu_count: u32) -> Self {
        Self {
            header: Header {
                version: VERSION,
                ram_size,
                ram_base,
                vcpu_count,
            },
            sections: Vec::new(),
        }
    }

    pub fn add_bincode<T: serde::Serialize>(
        &mut self,
        id: SectionId,
        index: u32,
        value: &T,
    ) -> Result<()> {
        let bytes = bincode::serialize(value)?;
        self.sections.push((id as u32, index, bytes));
        Ok(())
    }

    pub fn add_raw(&mut self, id: SectionId, index: u32, bytes: Vec<u8>) {
        self.sections.push((id as u32, index, bytes));
    }

    pub fn write_to_dir(self, dir: &Path) -> Result<()> {
        let path = vmstate_path(dir);
        // Snapshot state can embed guest secrets; keep it owner-only.
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;

        let num_sections = self.sections.len() as u32;
        let toc_len = (num_sections as usize) * TOC_ENTRY_LEN;
        let data_offset = (HEADER_LEN + toc_len) as u64;
        let mut toc_buf = vec![0u8; toc_len];
        let mut cursor = data_offset;
        for (i, (id, index, bytes)) in self.sections.iter().enumerate() {
            let mut hasher = Sha256::new();
            hasher.update(bytes);
            let sha = hasher.finalize();
            let mut sha256 = [0u8; 32];
            sha256.copy_from_slice(&sha);
            TocEntry {
                id: *id,
                index: *index,
                offset: cursor,
                len: bytes.len() as u64,
                sha256,
            }
            .encode(&mut toc_buf[i * TOC_ENTRY_LEN..(i + 1) * TOC_ENTRY_LEN]);
            cursor += bytes.len() as u64;
        }

        file.write_all(&self.header.encode(num_sections))?;
        file.write_all(&toc_buf)?;
        for (_, _, bytes) in &self.sections {
            file.write_all(bytes)?;
        }
        if snapshot_sync_enabled() {
            file.sync_all()?;
        }
        Ok(())
    }
}

pub struct SnapshotReader {
    #[allow(dead_code)]
    pub header: Header,
    sections: HashMap<(u32, u32), Vec<u8>>,
}

impl SnapshotReader {
    pub fn open(dir: &Path) -> Result<Self> {
        let path = vmstate_path(dir);
        let mut file = File::open(&path)?;
        let file_len = file.metadata()?.len();

        let mut hdr_buf = [0u8; HEADER_LEN];
        file.read_exact(&mut hdr_buf).map_err(|e| {
            SnapshotError::Io(std::io::Error::other(format!(
                "read {} header: {e}",
                path.display()
            )))
        })?;
        let (header, num_sections) = Header::decode(&hdr_buf)?;

        let toc_len = (num_sections as usize) * TOC_ENTRY_LEN;
        let mut toc_buf = vec![0u8; toc_len];
        file.read_exact(&mut toc_buf).map_err(|e| {
            SnapshotError::Io(std::io::Error::other(format!(
                "read {} toc len={toc_len}: {e}",
                path.display()
            )))
        })?;

        let mut entries = Vec::with_capacity(num_sections as usize);
        for i in 0..num_sections as usize {
            let entry = TocEntry::decode(&toc_buf[i * TOC_ENTRY_LEN..(i + 1) * TOC_ENTRY_LEN])?;
            let end = entry
                .offset
                .checked_add(entry.len)
                .ok_or(SnapshotError::Truncated)?;
            if end > file_len {
                return Err(SnapshotError::Truncated);
            }
            entries.push(entry);
        }

        let mut sections = HashMap::new();
        for entry in &entries {
            file.seek(SeekFrom::Start(entry.offset))?;
            let mut buf = vec![0u8; entry.len as usize];
            file.read_exact(&mut buf).map_err(|e| {
                SnapshotError::Io(std::io::Error::other(format!(
                    "read {} section id={} index={} offset={} len={}: {e}",
                    path.display(),
                    entry.id,
                    entry.index,
                    entry.offset,
                    entry.len
                )))
            })?;
            let mut hasher = Sha256::new();
            hasher.update(&buf);
            let sha = hasher.finalize();
            if sha.as_slice() != entry.sha256 {
                return Err(SnapshotError::BadHash {
                    id: entry.id,
                    index: entry.index,
                });
            }
            sections.insert((entry.id, entry.index), buf);
        }

        Ok(Self { header, sections })
    }

    pub fn get_raw(&self, id: SectionId, index: u32) -> Result<&[u8]> {
        self.sections
            .get(&(id as u32, index))
            .map(|v| v.as_slice())
            .ok_or(SnapshotError::SectionMissing {
                id: id as u32,
                index,
            })
    }

    pub fn get_bincode<T: serde::de::DeserializeOwned>(
        &self,
        id: SectionId,
        index: u32,
    ) -> Result<T> {
        Ok(bincode::deserialize(self.get_raw(id, index)?)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_round_trip_through_vmstate_file() {
        let dir = std::env::temp_dir().join(format!("lnx-vmstate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let mut writer = SnapshotWriter::new(0x4000_0000, 0x8000_0000, 2);
        writer.add_raw(SectionId::Vcpu, 0, vec![1, 2, 3]);
        writer.add_raw(SectionId::Vcpu, 1, vec![4, 5]);
        writer
            .add_bincode(SectionId::Meta, 0, &("meta".to_string(), 7u64))
            .expect("add meta");
        writer.write_to_dir(&dir).expect("write");

        let reader = SnapshotReader::open(&dir).expect("open");
        assert_eq!(reader.header.version, VERSION);
        assert_eq!(reader.header.ram_size, 0x4000_0000);
        assert_eq!(reader.header.ram_base, 0x8000_0000);
        assert_eq!(reader.header.vcpu_count, 2);
        assert_eq!(reader.get_raw(SectionId::Vcpu, 0).expect("vcpu0"), &[1, 2, 3]);
        assert_eq!(reader.get_raw(SectionId::Vcpu, 1).expect("vcpu1"), &[4, 5]);
        let meta: (String, u64) = reader.get_bincode(SectionId::Meta, 0).expect("meta");
        assert_eq!(meta, ("meta".to_string(), 7));
        assert!(matches!(
            reader.get_raw(SectionId::HvfGic, 0),
            Err(SnapshotError::SectionMissing { .. })
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_section_fails_hash_check() {
        let dir = std::env::temp_dir().join(format!("lnx-vmstate-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let mut writer = SnapshotWriter::new(0, 0, 1);
        writer.add_raw(SectionId::Vcpu, 0, vec![9; 64]);
        writer.write_to_dir(&dir).expect("write");

        let path = vmstate_path(&dir);
        let mut bytes = std::fs::read(&path).expect("read vmstate");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&path, bytes).expect("rewrite vmstate");

        assert!(matches!(
            SnapshotReader::open(&dir),
            Err(SnapshotError::BadHash { .. })
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
