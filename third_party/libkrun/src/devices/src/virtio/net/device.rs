// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.
use crate::Error as DeviceError;
use crate::virtio::net::Result;
use crate::virtio::net::{NUM_QUEUES, QUEUE_CONFIG};
use crate::virtio::queue::Error as QueueError;
use crate::virtio::{
    ActivateError, ActivateResult, DeviceQueue, DeviceSnapshot, DeviceSnapshotError, DeviceState,
    InterruptTransport, QueueConfig, TYPE_NET, VirtioDevice,
};

use super::backend::{ReadError, WriteError};
use super::worker::{NetWorker, NetWorkerStopResult};

use serde::{Deserialize, Serialize};
use std::thread::JoinHandle;
use utils::eventfd::EventFd;

use std::cmp;
use std::io::Write;
use std::os::fd::RawFd;
use std::path::PathBuf;
use virtio_bindings::virtio_net::VIRTIO_NET_F_MAC;
use virtio_bindings::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
use vm_memory::{ByteValued, GuestMemoryError, GuestMemoryMmap};

const VIRTIO_F_VERSION_1: u32 = 32;

#[derive(Debug)]
pub enum FrontendError {
    DescriptorChainTooSmall,
    EmptyQueue,
    GuestMemory(GuestMemoryError),
    QueueError(QueueError),
    ReadOnlyDescriptor,
}

#[derive(Debug)]
pub enum RxError {
    Backend(ReadError),
    DeviceError(DeviceError),
}

#[derive(Debug)]
pub enum TxError {
    Backend(WriteError),
    DeviceError(DeviceError),
    QueueError(QueueError),
}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
struct VirtioNetConfig {
    mac: [u8; 6],
    status: u16,
    max_virtqueue_pairs: u16,
}

// Safe because it only has data and has no implicit padding.
unsafe impl ByteValued for VirtioNetConfig {}

#[derive(Clone)]
pub enum VirtioNetBackend {
    UnixstreamFd(RawFd),
    UnixstreamPath(PathBuf),
    UnixgramFd(RawFd),
    UnixgramPath(PathBuf, bool),
    #[cfg(target_os = "linux")]
    Tap(String),
}

pub struct Net {
    id: String,
    pub cfg_backend: VirtioNetBackend,

    avail_features: u64,
    acked_features: u64,

    pub(crate) device_state: DeviceState,

    config: VirtioNetConfig,

    worker_thread: Option<(JoinHandle<NetWorkerStopResult>, EventFd)>,
    paused: Option<NetWorkerStopResult>,
}

impl Net {
    /// Create a new virtio network device using the backend
    pub fn new(
        id: String,
        cfg_backend: VirtioNetBackend,
        mac: [u8; 6],
        features: u32,
    ) -> Result<Self> {
        let avail_features = features as u64
            | (1 << VIRTIO_NET_F_MAC)
            | (1 << VIRTIO_RING_F_EVENT_IDX)
            | (1 << VIRTIO_F_VERSION_1);

        let config = VirtioNetConfig {
            mac,
            status: 0,
            max_virtqueue_pairs: 0,
        };

        Ok(Net {
            id,
            cfg_backend,

            avail_features,
            acked_features: 0u64,

            device_state: DeviceState::Inactive,
            config,

            worker_thread: None,
            paused: None,
        })
    }

    /// Provides the ID of this net device.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Provides the virtio-net backend of this net device.
    pub fn backend(&self) -> &VirtioNetBackend {
        &self.cfg_backend
    }
}

impl VirtioDevice for Net {
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
        TYPE_NET
    }

    fn device_name(&self) -> &str {
        "net"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &QUEUE_CONFIG
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
        log::warn!(
            "Net: guest driver attempted to write device config (offset={:x}, len={:x})",
            offset,
            data.len()
        );
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        let [rx_q, tx_q]: [_; NUM_QUEUES] = queues.try_into().map_err(|_| {
            error!("Cannot perform activate. Expected {} queue(s)", NUM_QUEUES);
            ActivateError::BadActivate
        })?;

        match NetWorker::new(
            rx_q,
            tx_q,
            interrupt.clone(),
            mem.clone(),
            self.acked_features,
            self.cfg_backend.clone(),
        ) {
            Ok(worker) => {
                let stop_fd = worker.stop_fd().try_clone().map_err(|err| {
                    error!("dup net stop_fd: {err}");
                    ActivateError::BadActivate
                })?;
                let handle = worker.run();
                self.worker_thread = Some((handle, stop_fd));
                self.device_state = DeviceState::Activated(mem, interrupt);
                Ok(())
            }
            Err(err) => {
                error!(
                    "Error activating virtio-net ({}) backend: {err:?}",
                    self.id()
                );
                Err(ActivateError::BadActivate)
            }
        }
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }

    fn pause(&mut self) -> std::result::Result<(), DeviceSnapshotError> {
        if self.paused.is_some() {
            return Ok(());
        }
        let (handle, stop_fd) = match self.worker_thread.take() {
            Some(t) => t,
            None => return Ok(()),
        };
        let _ = stop_fd.write(1);
        let res = handle
            .join()
            .map_err(|e| DeviceSnapshotError::Invalid(format!("net worker join: {e:?}")))?;
        self.paused = Some(res);
        Ok(())
    }

    fn resume(&mut self) -> std::result::Result<(), DeviceSnapshotError> {
        let stop = match self.paused.take() {
            Some(s) => s,
            None => return Ok(()),
        };
        let (mem, interrupt) = match &self.device_state {
            DeviceState::Activated(m, i) => (m.clone(), i.clone()),
            DeviceState::Inactive => {
                return Err(DeviceSnapshotError::Invalid(
                    "net resume on inactive device".into(),
                ));
            }
        };
        let worker = NetWorker::from_parts(stop.rx_q, stop.tx_q, interrupt, mem, stop.backend)
            .map_err(|e| DeviceSnapshotError::Invalid(format!("net resume: {e:?}")))?;
        let stop_fd = worker
            .stop_fd()
            .try_clone()
            .map_err(|e| DeviceSnapshotError::Invalid(format!("dup net stop_fd: {e}")))?;
        let handle = worker.run();
        self.worker_thread = Some((handle, stop_fd));
        Ok(())
    }

    fn serialize_state(&self) -> std::result::Result<DeviceSnapshot, DeviceSnapshotError> {
        let stop = self
            .paused
            .as_ref()
            .ok_or_else(|| DeviceSnapshotError::Invalid("net serialize before pause".into()))?;
        let queues = vec![stop.rx_q.queue.to_state(), stop.tx_q.queue.to_state()];
        let body = NetSnapshotBody {
            mac: self.config.mac,
            acked_features: self.acked_features,
        };
        let payload =
            bincode::serialize(&body).map_err(|e| DeviceSnapshotError::Codec(e.to_string()))?;
        Ok(DeviceSnapshot { queues, payload })
    }

    fn restore_state(
        &mut self,
        snap: &DeviceSnapshot,
    ) -> std::result::Result<(), DeviceSnapshotError> {
        let stop = self
            .paused
            .as_mut()
            .ok_or_else(|| DeviceSnapshotError::Invalid("net restore before pause".into()))?;
        if snap.queues.len() != 2 {
            return Err(DeviceSnapshotError::Invalid(format!(
                "net: expected 2 queues, got {}",
                snap.queues.len()
            )));
        }
        let body: NetSnapshotBody = bincode::deserialize(&snap.payload)
            .map_err(|e| DeviceSnapshotError::Codec(e.to_string()))?;
        if body.mac != self.config.mac {
            warn!(
                "net.restore.mac_mismatch current={:02x?} snapshot={:02x?}; using snapshot MAC",
                self.config.mac, body.mac
            );
            self.config.mac = body.mac;
        }
        self.acked_features = body.acked_features;
        stop.rx_q.queue.restore_state(&snap.queues[0]);
        stop.tx_q.queue.restore_state(&snap.queues[1]);
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct NetSnapshotBody {
    mac: [u8; 6],
    acked_features: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::net::QUEUE_SIZE;
    use crate::virtio::net::backend::NetBackend;
    use crate::virtio::queue::{Queue, QueueState};
    use std::sync::Arc;
    use utils::eventfd::{EFD_NONBLOCK, EventFd};

    struct DummyBackend;

    impl NetBackend for DummyBackend {
        fn read_frame(&mut self, _buf: &mut [u8]) -> std::result::Result<usize, ReadError> {
            Err(ReadError::NothingRead)
        }

        fn write_frame(
            &mut self,
            _hdr_len: usize,
            _buf: &mut [u8],
        ) -> std::result::Result<(), WriteError> {
            Err(WriteError::NothingWritten)
        }

        fn has_unfinished_write(&self) -> bool {
            false
        }

        fn try_finish_write(
            &mut self,
            _hdr_len: usize,
            _buf: &[u8],
        ) -> std::result::Result<(), WriteError> {
            Ok(())
        }

        fn raw_socket_fd(&self) -> RawFd {
            -1
        }
    }

    fn device_queue() -> DeviceQueue {
        DeviceQueue::new(
            Queue::new(QUEUE_SIZE),
            Arc::new(EventFd::new(EFD_NONBLOCK).expect("eventfd")),
        )
    }

    fn queue_state(next_avail: u16, next_used: u16) -> QueueState {
        QueueState {
            max_size: QUEUE_SIZE,
            size: QUEUE_SIZE,
            ready: true,
            desc_table: 0x1000,
            avail_ring: 0x2000,
            used_ring: 0x3000,
            next_avail,
            next_used,
            event_idx_enabled: true,
            num_added: 7,
        }
    }

    #[test]
    fn restore_state_adopts_snapshot_mac_for_forked_instance() {
        let current_mac = [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee];
        let snapshot_mac = [0x02, 0x10, 0x20, 0x30, 0x40, 0x50];
        let mut net = Net::new(
            "net".to_string(),
            VirtioNetBackend::UnixstreamPath(PathBuf::from("/unused")),
            current_mac,
            0,
        )
        .expect("net");
        net.paused = Some(NetWorkerStopResult {
            rx_q: device_queue(),
            tx_q: device_queue(),
            backend: Box::new(DummyBackend),
        });
        let rx = queue_state(11, 5);
        let tx = queue_state(13, 7);
        let payload = bincode::serialize(&NetSnapshotBody {
            mac: snapshot_mac,
            acked_features: 0x1234,
        })
        .expect("payload");
        let snap = DeviceSnapshot {
            queues: vec![rx.clone(), tx.clone()],
            payload,
        };

        net.restore_state(&snap).expect("restore net snapshot");

        assert_eq!(net.config.mac, snapshot_mac);
        assert_eq!(net.acked_features, 0x1234);
        let paused = net.paused.as_ref().expect("paused net worker");
        assert_eq!(paused.rx_q.queue.to_state().next_avail, rx.next_avail);
        assert_eq!(paused.tx_q.queue.to_state().next_used, tx.next_used);
    }
}
