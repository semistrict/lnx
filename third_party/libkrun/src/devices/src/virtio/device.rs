// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{ActivateResult, InterruptTransport, Queue, QueueState};
use crate::virtio::AsAny;
use utils::eventfd::EventFd;
use vm_memory::GuestMemoryMmap;

/// Errors returned from device snapshot/restore.
#[derive(Debug)]
pub enum DeviceSnapshotError {
    /// This device does not implement snapshot/restore in this libkrun version.
    Unsupported(String),
    /// Caller-visible reason to refuse a snapshot (e.g. vsock has open connections).
    Refused(String),
    /// Underlying serialization failure.
    Codec(String),
    /// State payload was invalid or inconsistent.
    Invalid(String),
}

impl std::fmt::Display for DeviceSnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceSnapshotError::Unsupported(d) => {
                write!(f, "device {d} does not support snapshot")
            }
            DeviceSnapshotError::Refused(s) => write!(f, "{s}"),
            DeviceSnapshotError::Codec(s) => write!(f, "device snapshot codec error: {s}"),
            DeviceSnapshotError::Invalid(s) => write!(f, "device snapshot invalid: {s}"),
        }
    }
}

impl std::error::Error for DeviceSnapshotError {}

/// Per-device snapshot blob. `queues` is queue cursor state (one per
/// virtqueue, in queue index order). `payload` is device-specific
/// bincode-encoded bytes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceSnapshot {
    pub queues: Vec<QueueState>,
    pub payload: Vec<u8>,
}

/// Configuration for a single virtqueue.
/// This is used by devices to declare their queue requirements,
/// and by the transport to construct the actual queues.
#[derive(Clone, Copy, Debug)]
pub struct QueueConfig {
    /// Maximum size of the queue.
    pub size: u16,
}

impl QueueConfig {
    pub const fn new(size: u16) -> Self {
        Self { size }
    }
}

/// A virtqueue combined with its notification eventfd.
/// This is passed to devices during activation.
pub struct DeviceQueue {
    pub queue: Queue,
    pub event: Arc<EventFd>,
}

impl DeviceQueue {
    pub fn new(queue: Queue, event: Arc<EventFd>) -> Self {
        Self { queue, event }
    }
}

/// Enum that indicates if a VirtioDevice is inactive or has been activated
/// and memory attached to it.
pub enum DeviceState {
    Inactive,
    Activated(GuestMemoryMmap, InterruptTransport),
}

impl DeviceState {
    pub fn signal_used_queue(&self) {
        match self {
            Self::Inactive => {
                warn!("DeviceState::signal_used_queue() called, but device is not activated")
            }
            Self::Activated(_, interrupt) => interrupt.signal_used_queue(),
        }
    }
}

impl DeviceState {
    pub fn is_activated(&self) -> bool {
        matches!(self, DeviceState::Activated(..))
    }
}

#[derive(Clone)]
pub struct VirtioShmRegion {
    pub host_addr: u64,
    pub guest_addr: u64,
    pub size: usize,
}

/// Trait for virtio devices to be driven by a virtio transport.
///
/// The lifecycle of a virtio device is to be moved to a virtio transport, which will then query the
/// device. The transport constructs queues based on queue_config() and passes them to the device
/// during activation, transferring ownership. After reset, the transport recreates queues
/// from queue_config() for the next negotiation cycle.
pub trait VirtioDevice: AsAny + Send {
    /// Get the available features offered by device.
    fn avail_features(&self) -> u64;

    /// Get acknowledged features of the driver.
    fn acked_features(&self) -> u64;

    /// Set acknowledged features of the driver.
    /// This function must maintain the following invariant:
    /// - self.avail_features() & self.acked_features() = self.get_acked_features()
    fn set_acked_features(&mut self, acked_features: u64);

    /// The virtio device type.
    fn device_type(&self) -> u32;

    /// Device name used for logging information about the device at the transport layer
    fn device_name(&self) -> &str;

    /// Returns the queue configuration for this device.
    /// The transport uses this to construct the queues during initialization and after reset.
    fn queue_config(&self) -> &[QueueConfig];

    /// The set of feature bits shifted by `page * 32`.
    fn avail_features_by_page(&self, page: u32) -> u32 {
        let avail_features = self.avail_features();
        match page {
            // Get the lower 32-bits of the features bitfield.
            0 => avail_features as u32,
            // Get the upper 32-bits of the features bitfield.
            1 => (avail_features >> 32) as u32,
            _ => {
                warn!("Received request for unknown features page.");
                0u32
            }
        }
    }

    /// Acknowledges that this set of features should be enabled.
    fn ack_features_by_page(&mut self, page: u32, value: u32) {
        let mut v = match page {
            0 => u64::from(value),
            1 => u64::from(value) << 32,
            _ => {
                warn!("Cannot acknowledge unknown features page: {page}");
                0u64
            }
        };

        // Check if the guest is ACK'ing a feature that we didn't claim to have.
        let avail_features = self.avail_features();
        let unrequested_features = v & !avail_features;
        if unrequested_features != 0 {
            warn!("Received acknowledge request for unknown feature: {v:x}");
            // Don't count these features as acked.
            v &= !unrequested_features;
        }
        self.set_acked_features(self.acked_features() | v);
    }

    /// Reads this device configuration space at `offset`.
    fn read_config(&self, offset: u64, data: &mut [u8]);

    /// Writes to this device configuration space at `offset`.
    fn write_config(&mut self, offset: u64, data: &[u8]);

    /// Performs the formal activation for a device, which can be verified also with `is_activated`.
    /// Ownership of the queues is transferred to the device.
    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult;

    /// Checks if the resources of this device are activated.
    fn is_activated(&self) -> bool;

    /// Optionally deactivates this device. The device should drop its queues.
    /// After reset, the transport will recreate queues from queue_config().
    fn reset(&mut self) -> bool {
        false
    }

    /// Get base and size of the SHM region
    fn shm_region(&self) -> Option<&VirtioShmRegion> {
        None
    }

    // ---- Snapshot / restore ----
    //
    // Devices in the v1 snapshot scope (block, vsock, net) override these.
    // Everything else uses the default `Unsupported` implementations so an
    // attempt to snapshot a VM containing an out-of-scope device returns a
    // clear error rather than silently producing a corrupt state.

    /// Quiesce the device's worker(s). After this returns, no further DMA into
    /// guest memory will occur until `resume` or `restore_state` is called.
    /// Drain any in-flight requests first.
    fn pause(&mut self) -> Result<(), DeviceSnapshotError> {
        Err(DeviceSnapshotError::Unsupported(
            self.device_name().to_string(),
        ))
    }

    /// Reverse of `pause`.
    fn resume(&mut self) -> Result<(), DeviceSnapshotError> {
        Err(DeviceSnapshotError::Unsupported(
            self.device_name().to_string(),
        ))
    }

    /// Serialize the device's snapshot. Must be called while the device is paused.
    fn serialize_state(&self) -> Result<DeviceSnapshot, DeviceSnapshotError> {
        Err(DeviceSnapshotError::Unsupported(
            self.device_name().to_string(),
        ))
    }

    /// Apply a previously-captured snapshot. Caller has already constructed the
    /// device, attached its backend resources, and `activate`d it; this
    /// rewinds the queue cursors and device-specific state to match the
    /// snapshot.
    fn restore_state(&mut self, _snap: &DeviceSnapshot) -> Result<(), DeviceSnapshotError> {
        Err(DeviceSnapshotError::Unsupported(
            self.device_name().to_string(),
        ))
    }
}

pub trait VmmExitObserver: Send {
    /// Callback to finish processing or cleanup the device resources
    fn on_vmm_exit(&mut self) {}
}

impl<F: Fn() + Send> VmmExitObserver for F {
    fn on_vmm_exit(&mut self) {
        self()
    }
}

impl std::fmt::Debug for dyn VirtioDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "VirtioDevice type {}", self.device_type())
    }
}
