// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use utils::byte_order;
use utils::eventfd::EventFd;
use vm_memory::{Address, Bytes, GuestMemoryMmap};

use super::super::{
    ActivateError, ActivateResult, DeviceQueue, DeviceSnapshot, DeviceSnapshotError, DeviceState,
    Queue as VirtQueue, QueueConfig, VirtioDevice,
};
use super::TsiFlags;
use super::muxer::VsockMuxer;
use super::packet::VsockPacket;
use super::tsi_stream::StreamListenerSnapshot;
use super::{defs, defs::uapi};
use crate::virtio::InterruptTransport;

use serde::{Deserialize, Serialize};

pub(crate) const RXQ_INDEX: usize = 0;
pub(crate) const TXQ_INDEX: usize = 1;
pub(crate) const EVQ_INDEX: usize = 2;

/// The virtio features supported by our vsock device:
/// - VIRTIO_F_VERSION_1: the device conforms to at least version 1.0 of the VirtIO spec.
/// - VIRTIO_F_IN_ORDER: the device returns used buffers in the same order that the driver makes
///   them available.
pub(crate) const AVAIL_FEATURES: u64 = (1 << uapi::VIRTIO_F_VERSION_1 as u64)
    | (1 << uapi::VIRTIO_F_IN_ORDER as u64)
    | (1 << uapi::VIRTIO_VSOCK_F_DGRAM);

pub struct Vsock {
    cid: u64,
    pub(crate) muxer: VsockMuxer,
    pub(crate) queue_rx: Option<Arc<Mutex<VirtQueue>>>,
    pub(crate) queue_tx: Option<Arc<Mutex<VirtQueue>>>,
    pub(crate) queue_ev: Option<Arc<Mutex<VirtQueue>>>,
    // Queue events are stored separately for event handling.
    pub(crate) queue_events: Vec<Arc<EventFd>>,
    pub(crate) avail_features: u64,
    pub(crate) acked_features: u64,
    pub(crate) activate_evt: EventFd,
    pub(crate) device_state: DeviceState,
    pending_transport_reset: bool,
}

impl Vsock {
    /// Create a new virtio-vsock device with the given VM CID.
    pub fn new(
        cid: u64,
        host_port_map: Option<HashMap<u16, u16>>,
        unix_ipc_port_map: Option<HashMap<u32, (PathBuf, bool)>>,
        tsi_flags: TsiFlags,
    ) -> super::Result<Vsock> {
        Ok(Vsock {
            cid,
            muxer: VsockMuxer::new(cid, host_port_map, unix_ipc_port_map, tsi_flags),
            queue_rx: None,
            queue_tx: None,
            queue_ev: None,
            queue_events: Vec::new(),
            avail_features: AVAIL_FEATURES,
            acked_features: 0,
            activate_evt: EventFd::new(utils::eventfd::EFD_NONBLOCK)
                .map_err(super::VsockError::EventFd)?,
            device_state: DeviceState::Inactive,
            pending_transport_reset: false,
        })
    }

    pub fn id(&self) -> &str {
        defs::VSOCK_DEV_ID
    }

    pub fn cid(&self) -> u64 {
        self.cid
    }

    /// Walk the driver-provided RX queue buffers and attempt to fill them up with any data that we
    /// have pending. Return `true` if descriptors have been added to the used ring, and `false`
    /// otherwise.
    pub fn process_stream_rx(&mut self) -> bool {
        debug!("process_stream_rx()");
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            // This should never happen, it's been already validated in the event handler.
            DeviceState::Inactive => unreachable!(),
        };

        let mut have_used = false;

        debug!("process_rx before while");
        let queue_rx = self
            .queue_rx
            .as_ref()
            .expect("queue_rx should exist when activated");
        let mut queue_rx = queue_rx.lock().unwrap();
        while let Some(head) = queue_rx.pop(mem) {
            debug!("process_rx inside while");
            let used_len = match VsockPacket::from_rx_virtq_head(&head) {
                Ok(mut pkt) => {
                    if self.muxer.recv_pkt(&mut pkt).is_ok() {
                        pkt.hdr().len() as u32 + pkt.len()
                    } else {
                        // We are using a consuming iterator over the virtio buffers, so, if we can't
                        // fill in this buffer, we'll need to undo the last iterator step.
                        queue_rx.undo_pop();
                        break;
                    }
                }
                Err(e) => {
                    warn!("RX queue error: {e:?}");
                    0
                }
            };

            debug!("process_rx: something to queue");
            have_used = true;
            if let Err(e) = queue_rx.add_used(mem, head.index, used_len) {
                error!("failed to add used elements to the queue: {e:?}");
            }
        }

        have_used
    }

    /// Walk the driver-provided TX queue buffers, package them up as vsock packets, and process
    /// them. Return `true` if descriptors have been added to the used ring, and `false` otherwise.
    pub fn process_stream_tx(&mut self) -> bool {
        debug!("process_stream_tx()");
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            // This should never happen, it's been already validated in the event handler.
            DeviceState::Inactive => unreachable!(),
        };

        let mut have_used = false;

        let queue_tx = self
            .queue_tx
            .as_ref()
            .expect("queue_tx should exist when activated");
        let mut queue_tx = queue_tx.lock().unwrap();
        while let Some(head) = queue_tx.pop(mem) {
            let pkt = match VsockPacket::from_tx_virtq_head(&head) {
                Ok(pkt) => pkt,
                Err(e) => {
                    error!("error reading TX packet: {e:?}");
                    have_used = true;
                    if let Err(e) = queue_tx.add_used(mem, head.index, 0) {
                        error!("failed to add used elements to the queue: {e:?}");
                    }
                    continue;
                }
            };
            if pkt.type_() == uapi::VSOCK_TYPE_DGRAM {
                debug!("process_stream_tx() is DGRAM");
                if self.muxer.send_dgram_pkt(&pkt).is_err() {
                    queue_tx.undo_pop();
                    break;
                }
            } else {
                debug!("process_stream_tx() is STREAM");
                if self.muxer.send_stream_pkt(&pkt).is_err() {
                    queue_tx.undo_pop();
                    break;
                }
            }

            have_used = true;
            if let Err(e) = queue_tx.add_used(mem, head.index, 0) {
                error!("failed to add used elements to the queue: {e:?}");
            }
        }

        have_used
    }

    fn write_transport_reset_event(&self) -> Result<bool, DeviceSnapshotError> {
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            DeviceState::Inactive => return Ok(false),
        };
        let Some(queue_ev) = self.queue_ev.as_ref() else {
            return Ok(false);
        };

        let mut queue_ev = queue_ev.lock().unwrap();
        let Some(head) = queue_ev.pop(mem) else {
            return Ok(false);
        };

        if !head.is_write_only() || head.len < 4 {
            warn!(
                "vsock event queue descriptor is not writable or too small: index={} len={}",
                head.index, head.len
            );
            if let Err(e) = queue_ev.add_used(mem, head.index, 0) {
                error!("failed to add malformed vsock event descriptor to used ring: {e:?}");
            }
            return Ok(true);
        }

        let event = uapi::VIRTIO_VSOCK_EVENT_TRANSPORT_RESET.to_le_bytes();
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            let _ = hvf::mark_dirty_ranges(&[(head.addr.raw_value(), event.len() as u64)]);
        }
        if let Err(e) = mem.write_slice(&event, head.addr) {
            queue_ev.undo_pop();
            return Err(DeviceSnapshotError::Invalid(format!(
                "vsock transport reset event write: {e:?}"
            )));
        }
        if let Err(e) = queue_ev.add_used(mem, head.index, event.len() as u32) {
            return Err(DeviceSnapshotError::Invalid(format!(
                "vsock transport reset event used ring: {e:?}"
            )));
        }

        info!("vsock.restore.transport_reset_event");
        Ok(true)
    }

    pub(crate) fn process_transport_reset_event(&mut self) -> bool {
        if !self.pending_transport_reset {
            return false;
        }

        match self.write_transport_reset_event() {
            Ok(wrote) => {
                if wrote {
                    self.pending_transport_reset = false;
                }
                wrote
            }
            Err(e) => {
                error!("failed to write pending vsock transport reset event: {e:?}");
                false
            }
        }
    }
}

impl VirtioDevice for Vsock {
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
        uapi::VIRTIO_ID_VSOCK
    }

    fn device_name(&self) -> &str {
        "vsock"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &defs::QUEUE_CONFIG
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        match offset {
            0 if data.len() == 8 => byte_order::write_le_u64(data, self.cid()),
            0 if data.len() == 4 => {
                byte_order::write_le_u32(data, (self.cid() & 0xffff_ffff) as u32)
            }
            4 if data.len() == 4 => {
                byte_order::write_le_u32(data, ((self.cid() >> 32) & 0xffff_ffff) as u32)
            }
            _ => warn!(
                "virtio-vsock received invalid read request of {} bytes at offset {}",
                data.len(),
                offset
            ),
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        warn!(
            "guest driver attempted to write device config (offset={:x}, len={:x})",
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

        // Store queue events for event handling.
        self.queue_events = queues.iter().map(|dq| dq.event.clone()).collect();

        // Extract queues from DeviceQueues and wrap in Arc<Mutex<>>.
        let mut queues_vec: Vec<VirtQueue> = queues.into_iter().map(|dq| dq.queue).collect();
        let ev_queue = queues_vec.pop().unwrap();
        let tx_queue = queues_vec.pop().unwrap();
        let rx_queue = queues_vec.pop().unwrap();

        self.queue_tx = Some(Arc::new(Mutex::new(tx_queue)));
        self.queue_rx = Some(Arc::new(Mutex::new(rx_queue)));
        self.queue_ev = Some(Arc::new(Mutex::new(ev_queue)));
        self.muxer.activate(
            mem.clone(),
            self.queue_rx.clone().unwrap(),
            interrupt.clone(),
        );

        self.device_state = DeviceState::Activated(mem, interrupt);

        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }

    fn pause(&mut self) -> Result<(), DeviceSnapshotError> {
        self.muxer.pause();
        Ok(())
    }

    fn resume(&mut self) -> Result<(), DeviceSnapshotError> {
        self.muxer.resume();
        let mut raise_irq = self.process_transport_reset_event();
        raise_irq |= self.process_stream_tx();
        if self.muxer.has_pending_rx() {
            raise_irq |= self.process_stream_rx();
        }
        if let DeviceState::Activated(_, ref interrupt) = self.device_state {
            if raise_irq {
                interrupt.signal_used_queue();
            }
        }
        Ok(())
    }

    fn serialize_state(&self) -> Result<DeviceSnapshot, DeviceSnapshotError> {
        let transport_reset = matches!(self.device_state, DeviceState::Activated(_, _));
        let mut queues = Vec::new();
        for q in [&self.queue_rx, &self.queue_tx, &self.queue_ev] {
            let q = q
                .as_ref()
                .ok_or_else(|| DeviceSnapshotError::Invalid("vsock not activated".into()))?;
            queues.push(q.lock().unwrap().to_state());
        }

        let body = VsockSnapshotBody {
            cid: self.cid,
            acked_features: self.acked_features,
            stream_listeners: self.muxer.stream_listener_snapshots(),
            pending_rx: Vec::new(),
            transport_reset,
        };
        let payload =
            bincode::serialize(&body).map_err(|e| DeviceSnapshotError::Codec(e.to_string()))?;
        Ok(DeviceSnapshot { queues, payload })
    }

    fn restore_state(&mut self, snap: &DeviceSnapshot) -> Result<(), DeviceSnapshotError> {
        if snap.queues.len() != 3 {
            return Err(DeviceSnapshotError::Invalid(format!(
                "vsock: expected 3 queues, got {}",
                snap.queues.len()
            )));
        }
        let body: VsockSnapshotBody = bincode::deserialize(&snap.payload)
            .map_err(|e| DeviceSnapshotError::Codec(e.to_string()))?;
        if body.cid != self.cid {
            return Err(DeviceSnapshotError::Invalid(format!(
                "vsock cid mismatch: snapshot={} current={}",
                body.cid, self.cid
            )));
        }
        self.acked_features = body.acked_features;
        if let Some(q) = &self.queue_rx {
            q.lock().unwrap().restore_state(&snap.queues[0]);
        }
        if let Some(q) = &self.queue_tx {
            q.lock().unwrap().restore_state(&snap.queues[1]);
        }
        if let Some(q) = &self.queue_ev {
            q.lock().unwrap().restore_state(&snap.queues[2]);
        }
        self.muxer
            .restore_unix_acceptors()
            .map_err(DeviceSnapshotError::Invalid)?;
        self.muxer
            .restore_stream_listeners(&body.stream_listeners)
            .map_err(DeviceSnapshotError::Invalid)?;
        self.pending_transport_reset = body.transport_reset || !body.pending_rx.is_empty();
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct VsockSnapshotBody {
    cid: u64,
    acked_features: u64,
    #[serde(default)]
    stream_listeners: Vec<StreamListenerSnapshot>,
    #[serde(default)]
    pending_rx: Vec<LegacyStreamReset>,
    #[serde(default)]
    transport_reset: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LegacyStreamReset {
    local_port: u32,
    peer_port: u32,
}
