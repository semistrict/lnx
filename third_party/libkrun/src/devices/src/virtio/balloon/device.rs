use std::cmp;
use std::convert::TryInto;
use std::io::Write;

use utils::eventfd::EventFd;
use vm_memory::{ByteValued, Bytes, GuestAddress, GuestMemory, GuestMemoryMmap};

use super::super::{
    ActivateError, ActivateResult, BalloonError, DeviceQueue, DeviceSnapshot, DeviceSnapshotError,
    DeviceState, QueueConfig, VirtioDevice,
};
use super::{defs, defs::uapi};
use crate::virtio::InterruptTransport;
use serde::{Deserialize, Serialize};

// Inflate queue.
pub(crate) const IFQ_INDEX: usize = 0;
// Deflate queue.
pub(crate) const DFQ_INDEX: usize = 1;
// Stats queue.
pub(crate) const STQ_INDEX: usize = 2;
// Free page reporting queue.
pub(crate) const FRQ_INDEX: usize = 3;

// Supported features.
pub(crate) const AVAIL_FEATURES: u64 = (1 << uapi::VIRTIO_F_VERSION_1 as u64)
    | (1 << uapi::VIRTIO_BALLOON_F_STATS_VQ as u64)
    | (1 << uapi::VIRTIO_BALLOON_F_REPORTING as u64);

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
pub struct VirtioBalloonConfig {
    /* Number of pages host wants Guest to give up. */
    num_pages: u32,
    /* Number of pages we've actually got in balloon. */
    actual: u32,
    /* Free page report command id, readonly by guest */
    free_page_report_cmd_id: u32,
    /* Stores PAGE_POISON if page poisoning is in use */
    poison_val: u32,
}

// Safe because it only has data and has no implicit padding.
unsafe impl ByteValued for VirtioBalloonConfig {}

pub struct Balloon {
    pub(crate) queues: Option<Vec<DeviceQueue>>,
    pub(crate) avail_features: u64,
    pub(crate) acked_features: u64,
    pub(crate) activate_evt: EventFd,
    pub(crate) device_state: DeviceState,
    config: VirtioBalloonConfig,
}

impl Balloon {
    pub fn new() -> super::Result<Balloon> {
        Ok(Balloon {
            queues: None,
            avail_features: AVAIL_FEATURES,
            acked_features: 0,
            activate_evt: EventFd::new(utils::eventfd::EFD_NONBLOCK)
                .map_err(BalloonError::EventFd)?,
            device_state: DeviceState::Inactive,
            config: VirtioBalloonConfig::default(),
        })
    }

    pub fn id(&self) -> &str {
        defs::BALLOON_DEV_ID
    }

    pub fn process_frq(&mut self) -> bool {
        debug!("balloon: process_frq()");
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            // This should never happen, it's been already validated in the event handler.
            DeviceState::Inactive => unreachable!(),
        };

        let queues = self
            .queues
            .as_mut()
            .expect("queues should exist when activated");
        let mut have_used = false;
        if !queues[FRQ_INDEX].queue.is_valid(mem) {
            let queue = &queues[FRQ_INDEX].queue;
            warn!(
                "balloon: free-page reporting queue is not ready ready={} size={} desc={:?} avail={:?} used={:?}",
                queue.ready, queue.size, queue.desc_table, queue.avail_ring, queue.used_ring
            );
            return false;
        }

        while let Some(head) = queues[FRQ_INDEX].queue.pop(mem) {
            let index = head.index;
            let mut reported_bytes = 0u64;
            for desc in head.into_iter() {
                let host_addr = mem.get_host_address(desc.addr).unwrap();
                debug!(
                    "balloon: should release guest_addr={:?} host_addr={:p} len={}",
                    desc.addr, host_addr, desc.len
                );
                #[cfg(target_os = "linux")]
                let advice = libc::MADV_DONTNEED;
                #[cfg(target_os = "macos")]
                let advice = libc::MADV_FREE_REUSABLE;
                let ret = unsafe {
                    libc::madvise(
                        host_addr as *mut libc::c_void,
                        desc.len.try_into().unwrap(),
                        advice,
                    )
                };
                if ret != 0 {
                    warn!(
                        "balloon: madvise failed for report guest_addr={:?}: {:?}",
                        desc.addr,
                        std::io::Error::last_os_error()
                    );
                } else {
                    reported_bytes += u64::from(desc.len);
                }
            }

            if reported_bytes > 0 {
                info!("balloon: reported {reported_bytes} bytes from free-page reporting queue");
            }
            have_used = true;
            if let Err(e) = queues[FRQ_INDEX].queue.add_used(mem, index, 0) {
                error!("failed to add used elements to the queue: {e:?}");
            }
        }

        have_used
    }

    fn release_guest_page(mem: &GuestMemoryMmap, page_frame_number: u32) {
        let guest_addr =
            GuestAddress(u64::from(page_frame_number) << defs::VIRTIO_BALLOON_PFN_SHIFT);
        let host_addr = match mem.get_host_address(guest_addr) {
            Ok(addr) => addr,
            Err(e) => {
                warn!("balloon: invalid inflate PFN {page_frame_number}: {e:?}");
                return;
            }
        };
        #[cfg(target_os = "linux")]
        let advice = libc::MADV_DONTNEED;
        #[cfg(target_os = "macos")]
        let advice = libc::MADV_FREE_REUSABLE;
        let page_size = 1usize << defs::VIRTIO_BALLOON_PFN_SHIFT;
        let ret = unsafe { libc::madvise(host_addr as *mut libc::c_void, page_size, advice) };
        if ret != 0 {
            warn!(
                "balloon: madvise failed for guest_addr={guest_addr:?}: {:?}",
                std::io::Error::last_os_error()
            );
        }
    }

    pub fn process_ifq(&mut self) -> bool {
        debug!("balloon: process_ifq()");
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            DeviceState::Inactive => unreachable!(),
        };

        let queues = self
            .queues
            .as_mut()
            .expect("queues should exist when activated");
        let mut have_used = false;
        if !queues[IFQ_INDEX].queue.is_valid(mem) {
            warn!("balloon: inflate queue is not ready");
            return false;
        }

        while let Some(head) = queues[IFQ_INDEX].queue.pop(mem) {
            let index = head.index;
            for desc in head.into_iter() {
                if desc.is_write_only() || desc.len % std::mem::size_of::<u32>() as u32 != 0 {
                    warn!("balloon: skipping malformed inflate descriptor");
                    continue;
                }
                let mut data = vec![0u8; desc.len as usize];
                if let Err(e) = mem.read(&mut data, desc.addr) {
                    warn!("balloon: failed to read inflate descriptor: {e:?}");
                    continue;
                }
                for chunk in data.chunks_exact(std::mem::size_of::<u32>()) {
                    let page_frame_number =
                        u32::from_le_bytes(chunk.try_into().expect("u32 chunk"));
                    Self::release_guest_page(mem, page_frame_number);
                }
            }

            have_used = true;
            if let Err(e) = queues[IFQ_INDEX].queue.add_used(mem, index, 0) {
                error!("failed to add used elements to the queue: {e:?}");
            }
        }

        have_used
    }

    pub fn process_dfq(&mut self) -> bool {
        debug!("balloon: process_dfq()");
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            DeviceState::Inactive => unreachable!(),
        };

        let queues = self
            .queues
            .as_mut()
            .expect("queues should exist when activated");
        let mut have_used = false;
        if !queues[DFQ_INDEX].queue.is_valid(mem) {
            warn!("balloon: deflate queue is not ready");
            return false;
        }

        while let Some(head) = queues[DFQ_INDEX].queue.pop(mem) {
            have_used = true;
            if let Err(e) = queues[DFQ_INDEX].queue.add_used(mem, head.index, 0) {
                error!("failed to add used elements to the queue: {e:?}");
            }
        }

        have_used
    }
}

impl VirtioDevice for Balloon {
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features
    }

    fn device_type(&self) -> u32 {
        uapi::VIRTIO_ID_BALLOON
    }

    fn device_name(&self) -> &str {
        "balloon"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &defs::QUEUE_CONFIG
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config_slice = self.config.as_slice();
        let config_len = config_slice.len() as u64;
        if offset >= config_len {
            error!("Failed to read config space");
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against config_len.
            data.write_all(&config_slice[offset as usize..cmp::min(end, config_len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        let config_slice = self.config.as_mut_slice();
        let Some(start) = usize::try_from(offset).ok() else {
            error!("Failed to write balloon config space");
            return;
        };
        let Some(end) = start.checked_add(data.len()) else {
            error!("Failed to write balloon config space");
            return;
        };
        let Some(dst) = config_slice.get_mut(start..end) else {
            error!("Failed to write balloon config space");
            return;
        };
        dst.copy_from_slice(data);
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        if queues.len() != defs::NUM_QUEUES {
            error!(
                "Cannot perform activate. Expected {} queue(s), got {}",
                defs::NUM_QUEUES,
                queues.len()
            );
            return Err(ActivateError::BadActivate);
        }

        if self.activate_evt.write(1).is_err() {
            error!("Cannot write to activate_evt",);
            return Err(ActivateError::BadActivate);
        }

        self.queues = Some(queues);
        self.device_state = DeviceState::Activated(mem, interrupt);

        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }

    fn pause(&mut self) -> Result<(), DeviceSnapshotError> {
        Ok(())
    }

    fn resume(&mut self) -> Result<(), DeviceSnapshotError> {
        Ok(())
    }

    fn serialize_state(&self) -> Result<DeviceSnapshot, DeviceSnapshotError> {
        let queues = self
            .queues
            .as_ref()
            .map(|queues| queues.iter().map(|q| q.queue.to_state()).collect())
            .unwrap_or_default();
        let body = BalloonSnapshotBody {
            acked_features: self.acked_features,
            config: self.config,
        };
        let payload =
            bincode::serialize(&body).map_err(|e| DeviceSnapshotError::Codec(e.to_string()))?;
        Ok(DeviceSnapshot { queues, payload })
    }

    fn restore_state(&mut self, snap: &DeviceSnapshot) -> Result<(), DeviceSnapshotError> {
        let body: BalloonSnapshotBody = bincode::deserialize(&snap.payload)
            .map_err(|e| DeviceSnapshotError::Codec(e.to_string()))?;
        self.acked_features = body.acked_features;
        self.config = body.config;
        if snap.queues.is_empty() {
            return Ok(());
        }
        let queues = self
            .queues
            .as_mut()
            .ok_or_else(|| DeviceSnapshotError::Invalid("balloon not activated".into()))?;
        if snap.queues.len() != queues.len() {
            return Err(DeviceSnapshotError::Invalid(format!(
                "balloon: expected {} queues, got {}",
                queues.len(),
                snap.queues.len()
            )));
        }
        for (queue, state) in queues.iter_mut().zip(&snap.queues) {
            queue.queue.restore_state(state);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct BalloonConfigSnapshot {
    num_pages: u32,
    actual: u32,
    free_page_report_cmd_id: u32,
    poison_val: u32,
}

impl From<VirtioBalloonConfig> for BalloonConfigSnapshot {
    fn from(config: VirtioBalloonConfig) -> Self {
        Self {
            num_pages: config.num_pages,
            actual: config.actual,
            free_page_report_cmd_id: config.free_page_report_cmd_id,
            poison_val: config.poison_val,
        }
    }
}

impl From<BalloonConfigSnapshot> for VirtioBalloonConfig {
    fn from(config: BalloonConfigSnapshot) -> Self {
        Self {
            num_pages: config.num_pages,
            actual: config.actual,
            free_page_report_cmd_id: config.free_page_report_cmd_id,
            poison_val: config.poison_val,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BalloonSnapshotBody {
    acked_features: u64,
    #[serde(
        serialize_with = "serialize_balloon_config",
        deserialize_with = "deserialize_balloon_config"
    )]
    config: VirtioBalloonConfig,
}

fn serialize_balloon_config<S>(
    config: &VirtioBalloonConfig,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    BalloonConfigSnapshot::from(*config).serialize(serializer)
}

fn deserialize_balloon_config<'de, D>(deserializer: D) -> Result<VirtioBalloonConfig, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(BalloonConfigSnapshot::deserialize(deserializer)?.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_is_writable_and_target_defaults_to_zero() {
        let mut balloon = Balloon::new().unwrap();

        let mut config = [0u8; std::mem::size_of::<VirtioBalloonConfig>()];
        balloon.read_config(0, &mut config);
        assert_eq!(u32::from_le_bytes(config[0..4].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(config[4..8].try_into().unwrap()), 0);

        balloon.write_config(4, &1234u32.to_le_bytes());
        balloon.read_config(0, &mut config);
        assert_eq!(u32::from_le_bytes(config[0..4].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(config[4..8].try_into().unwrap()), 1234);
    }
}
