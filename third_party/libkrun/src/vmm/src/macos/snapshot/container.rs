// vmstate.bin: a tagged-section blob.
//
// Layout, all little-endian:
//   header  (40 bytes)
//   toc     (num_sections * 56 bytes)
//   data    (sections concatenated)
//
// Header (40 bytes):
//   magic        [u8; 8]   = b"LKRNSS01"
//   version      u32       = current shared vmstate container version
//   num_sections u32
//   ram_size     u64
//   ram_base     u64
//   vcpu_count   u32
//   _reserved    u32
//
// TOC entry (56 bytes):
//   id           u32
//   index        u32
//   offset       u64       (offset of section bytes within vmstate.bin)
//   len          u64
//   sha256       [u8; 32]

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::snapshot_metadata::VMSTATE_VERSION;

use super::{Result, SnapshotError, snapshot_sync_enabled, vmstate_path};

const MAGIC: [u8; 8] = *b"LKRNSS01";
const VERSION: u32 = VMSTATE_VERSION;
const HEADER_LEN: usize = 40;
const TOC_ENTRY_LEN: usize = 56;

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SectionId {
    Meta = 1,
    Vcpu = 2,
    GicDist = 3,
    GicVcpu = 4,
    VirtioMmio = 5,
    HvfGic = 6,
    HvfGicDistRegs = 7,
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
        // [36..40] reserved
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
        Ok(TocEntry {
            id,
            index,
            offset,
            len,
            sha256,
        })
    }
}

/// Accumulates sections in memory, then writes vmstate.bin in one shot.
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

    /// Write vmstate.bin into `dir/vmstate.bin`. Caller is responsible for
    /// the atomic temp-dir + rename publish — this just writes the file.
    pub fn write_to_dir(self, dir: &Path) -> Result<()> {
        let path = vmstate_path(dir);
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;

        let num_sections = self.sections.len() as u32;
        let toc_len = (num_sections as usize) * TOC_ENTRY_LEN;
        let data_offset = (HEADER_LEN + toc_len) as u64;

        // Build TOC and compute offsets/hashes.
        let mut toc_buf = vec![0u8; toc_len];
        let mut cursor = data_offset;
        for (i, (id, index, bytes)) in self.sections.iter().enumerate() {
            let mut hasher = Sha256::new();
            hasher.update(bytes);
            let sha = hasher.finalize();
            let mut sha256 = [0u8; 32];
            sha256.copy_from_slice(&sha);

            let entry = TocEntry {
                id: *id,
                index: *index,
                offset: cursor,
                len: bytes.len() as u64,
                sha256,
            };
            entry.encode(&mut toc_buf[i * TOC_ENTRY_LEN..(i + 1) * TOC_ENTRY_LEN]);
            cursor += bytes.len() as u64;
        }

        // Header.
        let hdr_buf = self.header.encode(num_sections);
        file.write_all(&hdr_buf)?;
        file.write_all(&toc_buf)?;
        for (_, _, bytes) in &self.sections {
            file.write_all(bytes)?;
        }
        if snapshot_sync_enabled() {
            file.sync_all()?;
            crate::timing_event("snapshot.vmstate.synced");
        } else {
            crate::timing_event("snapshot.vmstate.sync.skipped");
        }
        Ok(())
    }
}

/// Reads vmstate.bin into an indexed in-memory map.
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
        file.read_exact(&mut hdr_buf)?;
        let (header, num_sections) = Header::decode(&hdr_buf)?;

        let toc_len = (num_sections as usize) * TOC_ENTRY_LEN;
        let mut toc_buf = vec![0u8; toc_len];
        file.read_exact(&mut toc_buf)?;

        let mut entries = Vec::with_capacity(num_sections as usize);
        for i in 0..num_sections as usize {
            let entry = TocEntry::decode(&toc_buf[i * TOC_ENTRY_LEN..(i + 1) * TOC_ENTRY_LEN])?;
            if entry.offset + entry.len > file_len {
                return Err(SnapshotError::Truncated);
            }
            entries.push(entry);
        }

        let mut sections = HashMap::new();
        for entry in &entries {
            file.seek(SeekFrom::Start(entry.offset))?;
            let mut buf = vec![0u8; entry.len as usize];
            file.read_exact(&mut buf)?;

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

        Ok(SnapshotReader { header, sections })
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
        let bytes = self.get_raw(id, index)?;
        Ok(bincode::deserialize(bytes)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Dummy {
        a: u64,
        b: Vec<u8>,
    }

    #[test]
    fn header_round_trip() {
        let h = Header {
            version: VERSION,
            ram_size: 0x4000_0000,
            ram_base: 0x4000_0000,
            vcpu_count: 4,
        };
        let buf = h.encode(7);
        let (h2, n) = Header::decode(&buf).unwrap();
        assert_eq!(h.ram_size, h2.ram_size);
        assert_eq!(h.ram_base, h2.ram_base);
        assert_eq!(h.vcpu_count, h2.vcpu_count);
        assert_eq!(n, 7);
    }

    #[test]
    fn bad_magic() {
        let mut buf = [0u8; HEADER_LEN];
        buf[0..8].copy_from_slice(b"NOPE0001");
        assert!(matches!(Header::decode(&buf), Err(SnapshotError::BadMagic)));
    }

    #[test]
    fn writer_reader_round_trip() {
        let tmp = tempdir();
        let mut w = SnapshotWriter::new(0x4000_0000, 0x4000_0000, 2);
        let d0 = Dummy {
            a: 0xdead_beef_dead_beefu64,
            b: vec![1, 2, 3, 4, 5],
        };
        let d1 = Dummy {
            a: 0xabad_1deau64,
            b: (0u8..200).collect(),
        };
        w.add_bincode(SectionId::Vcpu, 0, &d0).unwrap();
        w.add_bincode(SectionId::Vcpu, 1, &d1).unwrap();
        w.add_raw(SectionId::Meta, 0, vec![0xaa, 0xbb, 0xcc]);
        w.write_to_dir(&tmp).unwrap();

        let r = SnapshotReader::open(&tmp).unwrap();
        assert_eq!(r.header.vcpu_count, 2);
        let d0r: Dummy = r.get_bincode(SectionId::Vcpu, 0).unwrap();
        let d1r: Dummy = r.get_bincode(SectionId::Vcpu, 1).unwrap();
        assert_eq!(d0, d0r);
        assert_eq!(d1, d1r);
        assert_eq!(r.get_raw(SectionId::Meta, 0).unwrap(), &[0xaa, 0xbb, 0xcc]);
    }

    #[test]
    fn hash_tamper_detected() {
        let tmp = tempdir();
        let mut w = SnapshotWriter::new(0, 0, 1);
        w.add_raw(SectionId::Meta, 0, vec![1, 2, 3, 4]);
        w.write_to_dir(&tmp).unwrap();

        // Flip a byte in the section payload.
        let p = vmstate_path(&tmp);
        let mut bytes = std::fs::read(&p).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&p, &bytes).unwrap();

        match SnapshotReader::open(&tmp) {
            Err(SnapshotError::BadHash { .. }) => {}
            Err(e) => panic!("expected BadHash, got {e}"),
            Ok(_) => panic!("expected BadHash, got Ok"),
        }
    }

    fn tempdir() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("krun-snap-test-{pid}-{nanos}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
