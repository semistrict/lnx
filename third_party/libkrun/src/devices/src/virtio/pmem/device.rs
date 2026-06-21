use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use utils::eventfd::EventFd;
#[cfg(target_os = "macos")]
use vm_memory::Address;
use vm_memory::{Bytes, GuestMemoryMmap};

use super::defs;
use super::defs::uapi;
use crate::virtio::{
    ActivateError, ActivateResult, DeviceQueue, DeviceSnapshot, DeviceSnapshotError, DeviceState,
    InterruptTransport, QueueConfig, VirtioDevice, VirtioShmRegion,
};

pub(crate) const REQ_INDEX: usize = 0;
const VIRTIO_PMEM_F_SHMEM_REGION: u64 = 0;
const AVAIL_FEATURES: u64 =
    (1 << uapi::VIRTIO_F_VERSION_1 as u64) | (1 << VIRTIO_PMEM_F_SHMEM_REGION);

pub struct Pmem {
    id: String,
    guest_addr: u64,
    size: u64,
    file: File,
    queues: Option<Vec<DeviceQueue>>,
    shm_region: VirtioShmRegion,
    avail_features: u64,
    acked_features: u64,
    pub(crate) activate_evt: EventFd,
    pub(crate) device_state: DeviceState,
}

impl Pmem {
    pub fn new(id: String, path: &Path, guest_addr: u64, size: u64) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Self {
            id,
            guest_addr,
            size,
            file,
            queues: None,
            shm_region: VirtioShmRegion {
                host_addr: 0,
                guest_addr,
                size: size as usize,
            },
            avail_features: AVAIL_FEATURES,
            acked_features: 0,
            activate_evt: EventFd::new(utils::eventfd::EFD_NONBLOCK)?,
            device_state: DeviceState::Inactive,
        })
    }

    pub(crate) fn queue_event(&self, idx: usize) -> &std::sync::Arc<utils::eventfd::EventFd> {
        &self.queues.as_ref().expect("queues should exist")[idx].event
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Records the host virtual address of this device's DAX mapping so
    /// [`Self::flush_mapping`] can `msync(MS_SYNC)` it. Wired up once after
    /// guest memory is built (see `attach_pmem_devices`).
    pub fn set_host_addr(&mut self, host_addr: u64) {
        self.shm_region.host_addr = host_addr;
    }

    /// Makes all guest DAX writes durable on the backing file.
    ///
    /// The guest writes into the pmem window directly through the
    /// `mmap(MAP_SHARED)` DAX mapping created in `build_pmem_regions`. `fsync`
    /// alone does **not** flush pages dirtied purely by CPU stores through that
    /// mapping — the dirty state lives in the page tables, not the file's
    /// writeback set — so POSIX requires an explicit `msync(MS_SYNC)` over the
    /// mapped range before the `fsync`. `host_addr` is the page-aligned mmap
    /// base and `size` the mapped length.
    fn flush_mapping(&self) -> io::Result<()> {
        let host_addr = self.shm_region.host_addr;
        if host_addr != 0 {
            // SAFETY: `host_addr`/`size` describe this device's live DAX
            // mapping, established from its guest memory region at build time;
            // `host_addr` is the page-aligned mmap base and `size` its length.
            let ret = unsafe {
                libc::msync(
                    host_addr as *mut libc::c_void,
                    self.shm_region.size,
                    libc::MS_SYNC,
                )
            };
            if ret != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        self.file.sync_all()
    }

    fn flush(&self) -> io::Result<()> {
        self.flush_mapping()
    }

    pub fn process_req(&mut self) -> bool {
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            DeviceState::Inactive => unreachable!(),
        };

        let mut have_used = false;
        while let Some(head) = self.queues.as_mut().unwrap()[REQ_INDEX].queue.pop(mem) {
            let index = head.index;
            let mut req_type = None;
            let mut resp_addr = None;
            let mut resp_len = 0u32;

            for desc in head.into_iter() {
                if desc.is_read_only() {
                    if desc.len >= 4 {
                        match mem.read_obj::<u32>(desc.addr) {
                            Ok(value) => req_type = Some(u32::from_le(value)),
                            Err(e) => {
                                error!("pmem: failed to read request: {e:?}");
                                self.queues.as_mut().unwrap()[REQ_INDEX]
                                    .queue
                                    .go_to_previous_position();
                                return have_used;
                            }
                        }
                    }
                } else if desc.len >= 4 {
                    resp_addr = Some(desc.addr);
                    resp_len = 4;
                }
            }

            let ret = if req_type == Some(uapi::VIRTIO_PMEM_REQ_TYPE_FLUSH) {
                match self.flush() {
                    Ok(()) => 0u32,
                    Err(e) => {
                        error!("pmem: flush failed: {e}");
                        libc::EIO as u32
                    }
                }
            } else {
                libc::EINVAL as u32
            };

            if let Some(addr) = resp_addr {
                #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
                {
                    let _ = hvf::mark_dirty_ranges(&[(addr.raw_value(), resp_len as u64)]);
                }
                if let Err(e) = mem.write_obj(ret.to_le(), addr) {
                    error!("pmem: failed to write response: {e:?}");
                    self.queues.as_mut().unwrap()[REQ_INDEX]
                        .queue
                        .go_to_previous_position();
                    return have_used;
                }
            }

            have_used = true;
            if let Err(e) = self.queues.as_mut().unwrap()[REQ_INDEX]
                .queue
                .add_used(mem, index, resp_len)
            {
                error!("pmem: failed to add used element: {e:?}");
            }
        }

        have_used
    }
}

impl VirtioDevice for Pmem {
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features;
    }

    fn device_type(&self) -> u32 {
        uapi::VIRTIO_ID_PMEM
    }

    fn device_name(&self) -> &str {
        "pmem"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &defs::QUEUE_CONFIG
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut config = [0u8; 16];
        config[..8].copy_from_slice(&self.guest_addr.to_le_bytes());
        config[8..].copy_from_slice(&self.size.to_le_bytes());
        let offset = offset as usize;
        if offset >= config.len() {
            data.fill(0);
            return;
        }
        let count = data.len().min(config.len() - offset);
        data[..count].copy_from_slice(&config[offset..offset + count]);
        data[count..].fill(0);
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        warn!(
            "pmem: guest attempted to write config (offset={offset:x}, len={:x})",
            data.len()
        );
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        if queues.len() != defs::NUM_QUEUES {
            return Err(ActivateError::BadActivate);
        }
        self.activate_evt
            .write(1)
            .map_err(|_| ActivateError::BadActivate)?;
        self.queues = Some(queues);
        self.device_state = DeviceState::Activated(mem, interrupt);
        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }

    fn shm_region(&self) -> Option<&VirtioShmRegion> {
        Some(&self.shm_region)
    }

    fn reset(&mut self) -> bool {
        self.queues = None;
        self.device_state = DeviceState::Inactive;
        true
    }

    fn pause(&mut self) -> Result<(), DeviceSnapshotError> {
        // The snapshot orchestrator pauses every device before its paused hook
        // clones the backing rootfs. Flush guest DAX writes here so the clone
        // captures a coherent, fully-synced image — the host half of the
        // AGENTS.md pause+flush contract.
        self.flush_mapping()
            .map_err(|e| DeviceSnapshotError::Invalid(format!("pmem flush before pause: {e}")))?;
        Ok(())
    }

    fn resume(&mut self) -> Result<(), DeviceSnapshotError> {
        Ok(())
    }

    fn serialize_state(&self) -> Result<DeviceSnapshot, DeviceSnapshotError> {
        let queues = self
            .queues
            .as_ref()
            .ok_or_else(|| DeviceSnapshotError::Invalid("pmem not activated".into()))?
            .iter()
            .map(|q| q.queue.to_state())
            .collect();
        let payload = bincode::serialize(&PmemSnapshotBody {
            acked_features: self.acked_features,
        })
        .map_err(|e| DeviceSnapshotError::Codec(e.to_string()))?;
        Ok(DeviceSnapshot { queues, payload })
    }

    fn restore_state(&mut self, snap: &DeviceSnapshot) -> Result<(), DeviceSnapshotError> {
        let queues = self
            .queues
            .as_mut()
            .ok_or_else(|| DeviceSnapshotError::Invalid("pmem not activated".into()))?;
        if snap.queues.len() != queues.len() {
            return Err(DeviceSnapshotError::Invalid(format!(
                "pmem: expected {} queues, got {}",
                queues.len(),
                snap.queues.len()
            )));
        }
        let body: PmemSnapshotBody = bincode::deserialize(&snap.payload)
            .map_err(|e| DeviceSnapshotError::Codec(e.to_string()))?;
        self.acked_features = body.acked_features;
        for (queue, state) in queues.iter_mut().zip(&snap.queues) {
            queue.queue.restore_state(state);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PmemSnapshotBody {
    acked_features: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::io::AsRawFd;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("lnx-pmem-{tag}-{}.bin", std::process::id()))
    }

    fn make_backing_file(path: &Path, len: u64) {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        file.set_len(len).unwrap();
    }

    /// `flush` must `msync(MS_SYNC)` the wired DAX mapping and `fsync` the file
    /// without error, and bytes dirtied purely through CPU stores on the
    /// `MAP_SHARED` mapping must be readable through an independent descriptor.
    /// This guards the `host_addr`/size wiring (a bad base or length makes
    /// `msync` fail with `EINVAL`).
    #[test]
    fn flush_syncs_mmap_dirtied_pages() {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let path = temp_path("flush");
        make_backing_file(&path, page as u64);

        // Map the file shared exactly as `build_pmem_regions` does for the guest.
        let map_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let host_addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                page,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                map_file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(host_addr, libc::MAP_FAILED, "mmap failed");

        let mut pmem = Pmem::new("test".to_string(), &path, 0, page as u64).unwrap();
        pmem.set_host_addr(host_addr as u64);

        // Dirty the mapping through CPU stores — the DAX write path.
        let pattern = b"pmem-msync-durability";
        unsafe {
            std::ptr::copy_nonoverlapping(pattern.as_ptr(), host_addr as *mut u8, pattern.len());
        }

        pmem.flush()
            .expect("flush must msync + fsync without error");

        let mut observed = Vec::new();
        File::open(&path)
            .unwrap()
            .read_to_end(&mut observed)
            .unwrap();
        assert_eq!(&observed[..pattern.len()], pattern);

        unsafe {
            libc::munmap(host_addr, page);
        }
        let _ = std::fs::remove_file(&path);
    }

    /// A device whose mapping was never wired (`host_addr == 0`) must still
    /// `fsync` and must not call `msync` with a null base.
    #[test]
    fn flush_without_mapping_falls_back_to_fsync() {
        let path = temp_path("nomap");
        make_backing_file(&path, 4096);
        let pmem = Pmem::new("test".to_string(), &path, 0, 4096).unwrap();
        assert_eq!(pmem.shm_region.host_addr, 0);
        pmem.flush()
            .expect("flush should fsync even without a mapping");
        let _ = std::fs::remove_file(&path);
    }
}
