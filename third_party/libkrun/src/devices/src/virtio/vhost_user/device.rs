// Copyright 2026, Red Hat Inc. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Generic vhost-user device wrapper.
//!
//! This module provides a wrapper around the vhost crate's Frontend,
//! adapting it to work with libkrun's VirtioDevice trait.

use std::io::{self, ErrorKind, Read, Result as IoResult};
use std::mem;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::ptr;
use std::sync::{Arc, Mutex};

use log::{debug, error, warn};
use polly::event_manager::{EventManager, Subscriber};
use serde::{Deserialize, Serialize};
use utils::epoll::{EpollEvent, EventSet};
use utils::eventfd::{EFD_NONBLOCK, EventFd};
use vm_memory::{Address, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

use crate::virtio::{
    ActivateError, ActivateResult, DeviceQueue, DeviceSnapshot, DeviceSnapshotError, DeviceState,
    InterruptTransport, QueueConfig, VirtioDevice,
};

/// VHOST_USER_F_PROTOCOL_FEATURES (bit 30) is a backend-only feature
/// that enables vhost-user protocol extensions. It's not a virtio feature.
const VHOST_USER_F_PROTOCOL_FEATURES: u64 = 1 << 30;

const VHOST_USER_HEADER_VERSION: u32 = 0x1;
const VHOST_USER_HEADER_REPLY: u32 = 0x4;
const VHOST_USER_HEADER_NEED_REPLY: u32 = 0x8;

const VHOST_USER_GET_FEATURES: u32 = 1;
const VHOST_USER_SET_FEATURES: u32 = 2;
const VHOST_USER_SET_OWNER: u32 = 3;
const VHOST_USER_SET_MEM_TABLE: u32 = 5;
const VHOST_USER_SET_VRING_NUM: u32 = 8;
const VHOST_USER_SET_VRING_ADDR: u32 = 9;
const VHOST_USER_SET_VRING_BASE: u32 = 10;
const VHOST_USER_GET_VRING_BASE: u32 = 11;
const VHOST_USER_SET_VRING_KICK: u32 = 12;
const VHOST_USER_SET_VRING_CALL: u32 = 13;
const VHOST_USER_GET_PROTOCOL_FEATURES: u32 = 15;
const VHOST_USER_SET_PROTOCOL_FEATURES: u32 = 16;
const VHOST_USER_GET_QUEUE_NUM: u32 = 17;
const VHOST_USER_SET_VRING_ENABLE: u32 = 18;
const VHOST_USER_GET_CONFIG: u32 = 24;
const VHOST_USER_SET_CONFIG: u32 = 25;
const VHOST_USER_ADD_MEM_REG: u32 = 37;

const VHOST_USER_PROTOCOL_F_MQ: u64 = 1 << 0;
const VHOST_USER_PROTOCOL_F_REPLY_ACK: u64 = 1 << 3;
const VHOST_USER_PROTOCOL_F_CONFIG: u64 = 1 << 9;
const VHOST_USER_PROTOCOL_F_CONFIGURE_MEM_SLOTS: u64 = 1 << 15;

const VHOST_USER_MAX_CONFIG_SIZE: usize = 256;
const VHOST_USER_BASELINE_MEMORY_REGIONS: usize = 8;

#[repr(C)]
#[derive(Clone, Copy)]
struct VhostUserMsgHeader {
    request: u32,
    flags: u32,
    size: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VhostVringState {
    index: u32,
    num: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VhostVringAddr {
    index: u32,
    flags: u32,
    desc_user_addr: u64,
    used_user_addr: u64,
    avail_user_addr: u64,
    log_guest_addr: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VhostUserMemoryRegion {
    guest_phys_addr: u64,
    memory_size: u64,
    user_addr: u64,
    mmap_offset: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VhostUserMemRegMsg {
    padding: u64,
    region: VhostUserMemoryRegion,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VhostUserConfig {
    offset: u32,
    size: u32,
    flags: u32,
    region: [u8; VHOST_USER_MAX_CONFIG_SIZE],
}

struct VhostUserMemoryRegionInfo {
    guest_phys_addr: u64,
    memory_size: u64,
    userspace_addr: u64,
    mmap_offset: u64,
    mmap_handle: RawFd,
}

impl VhostUserMemoryRegionInfo {
    fn to_region(&self) -> VhostUserMemoryRegion {
        VhostUserMemoryRegion {
            guest_phys_addr: self.guest_phys_addr,
            memory_size: self.memory_size,
            user_addr: self.userspace_addr,
            mmap_offset: self.mmap_offset,
        }
    }
}

struct Frontend {
    stream: UnixStream,
}

impl Frontend {
    fn from_stream(stream: UnixStream) -> Self {
        Self { stream }
    }

    fn get_features(&mut self) -> IoResult<u64> {
        self.request_u64(VHOST_USER_GET_FEATURES)
    }

    fn set_features(&mut self, features: u64, reply_ack: bool) -> IoResult<()> {
        self.send_u64(VHOST_USER_SET_FEATURES, features, reply_ack)
    }

    fn get_protocol_features(&mut self) -> IoResult<u64> {
        self.request_u64(VHOST_USER_GET_PROTOCOL_FEATURES)
    }

    fn set_protocol_features(&mut self, features: u64) -> IoResult<()> {
        self.send_u64(VHOST_USER_SET_PROTOCOL_FEATURES, features, false)
    }

    fn get_queue_num(&mut self) -> IoResult<u64> {
        self.request_u64(VHOST_USER_GET_QUEUE_NUM)
    }

    fn set_owner(&mut self, reply_ack: bool) -> IoResult<()> {
        self.send_request(VHOST_USER_SET_OWNER, &[], &[], reply_ack, reply_ack)
            .map(|_| ())
    }

    fn set_memory_regions(
        &mut self,
        regions: &[VhostUserMemoryRegionInfo],
        configure_mem_slots: bool,
        reply_ack: bool,
    ) -> IoResult<()> {
        if configure_mem_slots {
            for region in regions {
                self.add_mem_reg(region, reply_ack)?;
            }
            return Ok(());
        }

        if regions.len() > VHOST_USER_BASELINE_MEMORY_REGIONS {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                format!(
                    "backend does not support memory slots, but VM has {} regions",
                    regions.len()
                ),
            ));
        }

        let mut payload =
            Vec::with_capacity(8 + regions.len() * mem::size_of::<VhostUserMemoryRegion>());
        payload.extend_from_slice(&(regions.len() as u32).to_ne_bytes());
        payload.extend_from_slice(&0_u32.to_ne_bytes());
        for region in regions {
            payload.extend_from_slice(bytes_of(&region.to_region()));
        }
        let fds = regions
            .iter()
            .map(|region| region.mmap_handle)
            .collect::<Vec<_>>();
        self.send_request(
            VHOST_USER_SET_MEM_TABLE,
            &payload,
            &fds,
            reply_ack,
            reply_ack,
        )
        .map(|_| ())
    }

    fn add_mem_reg(&mut self, region: &VhostUserMemoryRegionInfo, reply_ack: bool) -> IoResult<()> {
        let payload = VhostUserMemRegMsg {
            padding: 0,
            region: region.to_region(),
        };
        self.send_request(
            VHOST_USER_ADD_MEM_REG,
            bytes_of(&payload),
            &[region.mmap_handle],
            reply_ack,
            reply_ack,
        )
        .map(|_| ())
    }

    fn set_vring_num(&mut self, index: usize, size: u16, reply_ack: bool) -> IoResult<()> {
        self.send_vring_state(VHOST_USER_SET_VRING_NUM, index, u32::from(size), reply_ack)
    }

    fn set_vring_base(&mut self, index: usize, base: u32, reply_ack: bool) -> IoResult<()> {
        self.send_vring_state(VHOST_USER_SET_VRING_BASE, index, base, reply_ack)
    }

    fn get_vring_base(&mut self, index: usize) -> IoResult<u32> {
        let payload = VhostVringState {
            index: index as u32,
            num: 0,
        };
        let reply = self
            .send_request(
                VHOST_USER_GET_VRING_BASE,
                bytes_of(&payload),
                &[],
                false,
                true,
            )?
            .ok_or_else(|| io::Error::new(ErrorKind::UnexpectedEof, "missing vring base reply"))?;
        if reply.len() < mem::size_of::<VhostVringState>() {
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "short vring base reply",
            ));
        }
        let state = VhostVringState {
            index: u32::from_ne_bytes(reply[0..4].try_into().unwrap()),
            num: u32::from_ne_bytes(reply[4..8].try_into().unwrap()),
        };
        if state.index != index as u32 {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!(
                    "wrong vring base reply index: expected {}, got {}",
                    index, state.index
                ),
            ));
        }
        Ok(state.num)
    }

    fn set_vring_addr(
        &mut self,
        index: usize,
        desc_table_addr: u64,
        used_ring_addr: u64,
        avail_ring_addr: u64,
        reply_ack: bool,
    ) -> IoResult<()> {
        let payload = VhostVringAddr {
            index: index as u32,
            flags: 0,
            desc_user_addr: desc_table_addr,
            used_user_addr: used_ring_addr,
            avail_user_addr: avail_ring_addr,
            log_guest_addr: 0,
        };
        self.send_request(
            VHOST_USER_SET_VRING_ADDR,
            bytes_of(&payload),
            &[],
            reply_ack,
            reply_ack,
        )
        .map(|_| ())
    }

    fn set_vring_kick(&mut self, index: usize, fd: RawFd, reply_ack: bool) -> IoResult<()> {
        self.send_u64_with_fds(VHOST_USER_SET_VRING_KICK, index as u64, &[fd], reply_ack)
    }

    fn set_vring_call(&mut self, index: usize, fd: RawFd, reply_ack: bool) -> IoResult<()> {
        self.send_u64_with_fds(VHOST_USER_SET_VRING_CALL, index as u64, &[fd], reply_ack)
    }

    fn set_vring_enable(&mut self, index: usize, enable: bool, reply_ack: bool) -> IoResult<()> {
        self.send_vring_state(
            VHOST_USER_SET_VRING_ENABLE,
            index,
            u32::from(enable),
            reply_ack,
        )
    }

    fn get_config(&mut self, offset: u32, size: u32, flags: u32) -> IoResult<Vec<u8>> {
        if size as usize > VHOST_USER_MAX_CONFIG_SIZE {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "vhost-user config read is too large",
            ));
        }
        let payload = VhostUserConfig {
            offset,
            size,
            flags,
            region: [0; VHOST_USER_MAX_CONFIG_SIZE],
        };
        let reply = self
            .send_request(VHOST_USER_GET_CONFIG, bytes_of(&payload), &[], false, true)?
            .ok_or_else(|| io::Error::new(ErrorKind::UnexpectedEof, "missing config reply"))?;
        if reply.len() < mem::size_of::<VhostUserConfig>() {
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "short config reply",
            ));
        }
        Ok(reply[12..12 + size as usize].to_vec())
    }

    fn set_config(
        &mut self,
        offset: u32,
        flags: u32,
        data: &[u8],
        reply_ack: bool,
    ) -> IoResult<()> {
        if data.len() > VHOST_USER_MAX_CONFIG_SIZE {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "vhost-user config write is too large",
            ));
        }
        let mut payload = VhostUserConfig {
            offset,
            size: data.len() as u32,
            flags,
            region: [0; VHOST_USER_MAX_CONFIG_SIZE],
        };
        payload.region[..data.len()].copy_from_slice(data);
        self.send_request(
            VHOST_USER_SET_CONFIG,
            bytes_of(&payload),
            &[],
            reply_ack,
            reply_ack,
        )
        .map(|_| ())
    }

    fn send_vring_state(
        &mut self,
        request: u32,
        index: usize,
        value: u32,
        reply_ack: bool,
    ) -> IoResult<()> {
        let payload = VhostVringState {
            index: index as u32,
            num: value,
        };
        self.send_request(request, bytes_of(&payload), &[], reply_ack, reply_ack)
            .map(|_| ())
    }

    fn request_u64(&mut self, request: u32) -> IoResult<u64> {
        let reply = self
            .send_request(request, &[], &[], false, true)?
            .ok_or_else(|| io::Error::new(ErrorKind::UnexpectedEof, "missing u64 reply"))?;
        if reply.len() < mem::size_of::<u64>() {
            return Err(io::Error::new(ErrorKind::UnexpectedEof, "short u64 reply"));
        }
        Ok(u64::from_ne_bytes(reply[..8].try_into().unwrap()))
    }

    fn send_u64(&mut self, request: u32, value: u64, reply_ack: bool) -> IoResult<()> {
        self.send_u64_with_fds(request, value, &[], reply_ack)
    }

    fn send_u64_with_fds(
        &mut self,
        request: u32,
        value: u64,
        fds: &[RawFd],
        reply_ack: bool,
    ) -> IoResult<()> {
        self.send_request(request, &value.to_ne_bytes(), fds, reply_ack, reply_ack)
            .map(|_| ())
    }

    fn send_request(
        &mut self,
        request: u32,
        payload: &[u8],
        fds: &[RawFd],
        need_reply: bool,
        read_reply: bool,
    ) -> IoResult<Option<Vec<u8>>> {
        let flags = VHOST_USER_HEADER_VERSION
            | if need_reply {
                VHOST_USER_HEADER_NEED_REPLY
            } else {
                0
            };
        let header = VhostUserMsgHeader {
            request,
            flags,
            size: payload.len() as u32,
        };
        send_with_fds(&self.stream, &[bytes_of(&header), payload], fds)?;
        if !read_reply {
            return Ok(None);
        }

        let reply = self.recv_reply(request)?;
        if need_reply && reply.len() >= mem::size_of::<u64>() {
            let status = u64::from_ne_bytes(reply[..8].try_into().unwrap());
            if status != 0 {
                return Err(io::Error::other(format!(
                    "vhost-user request {request} failed with status {status}"
                )));
            }
        }
        Ok(Some(reply))
    }

    fn recv_reply(&mut self, request: u32) -> IoResult<Vec<u8>> {
        let mut header_bytes = [0_u8; mem::size_of::<VhostUserMsgHeader>()];
        self.stream.read_exact(&mut header_bytes)?;
        let header = VhostUserMsgHeader {
            request: u32::from_ne_bytes(header_bytes[0..4].try_into().unwrap()),
            flags: u32::from_ne_bytes(header_bytes[4..8].try_into().unwrap()),
            size: u32::from_ne_bytes(header_bytes[8..12].try_into().unwrap()),
        };
        if header.request != request || header.flags & VHOST_USER_HEADER_REPLY == 0 {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!(
                    "unexpected vhost-user reply request={} flags=0x{:x}",
                    header.request, header.flags
                ),
            ));
        }
        let mut payload = vec![0_u8; header.size as usize];
        if !payload.is_empty() {
            self.stream.read_exact(&mut payload)?;
        }
        Ok(payload)
    }
}

fn bytes_of<T>(value: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(ptr::from_ref(value).cast::<u8>(), mem::size_of::<T>()) }
}

fn send_with_fds(stream: &UnixStream, bufs: &[&[u8]], fds: &[RawFd]) -> IoResult<()> {
    let mut iovecs = bufs
        .iter()
        .filter(|buf| !buf.is_empty())
        .map(|buf| libc::iovec {
            iov_base: buf.as_ptr().cast_mut().cast(),
            iov_len: buf.len(),
        })
        .collect::<Vec<_>>();
    let total_len = iovecs.iter().map(|iov| iov.iov_len).sum::<usize>();
    let fd_bytes = mem::size_of_val(fds);
    let mut control = if fds.is_empty() {
        Vec::new()
    } else {
        vec![0_u8; unsafe { libc::CMSG_SPACE(fd_bytes as _) as usize }]
    };

    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = iovecs.as_mut_ptr();
    msg.msg_iovlen = iovecs.len() as _;
    if !fds.is_empty() {
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = control.len() as _;
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            if cmsg.is_null() {
                return Err(io::Error::other("failed to create fd control message"));
            }
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(fd_bytes as _) as _;
            ptr::copy_nonoverlapping(
                fds.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(cmsg).cast::<u8>(),
                fd_bytes,
            );
            msg.msg_controllen = libc::CMSG_SPACE(fd_bytes as _) as _;
        }
    }

    loop {
        let written = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, 0) };
        if written < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if written as usize != total_len {
            return Err(io::Error::new(
                ErrorKind::WriteZero,
                format!("short vhost-user write: {written} of {total_len} bytes"),
            ));
        }
        return Ok(());
    }
}

#[cfg(target_os = "macos")]
fn eventfd_write_fd(event: &EventFd) -> RawFd {
    event.get_write_fd()
}

#[cfg(not(target_os = "macos"))]
fn eventfd_write_fd(event: &EventFd) -> RawFd {
    event.as_raw_fd()
}

/// Generic vhost-user device wrapper.
///
/// This wraps a vhost-user backend connection and implements the VirtioDevice
/// trait, allowing it to be used like any other virtio device in libkrun.
pub struct VhostUserDevice {
    /// Vhost-user frontend connection
    frontend: Arc<Mutex<Frontend>>,

    /// Device type (e.g., VIRTIO_ID_RNG = 4)
    device_type: u32,

    /// Device name for logging
    device_name: String,

    /// Queue configurations
    queue_configs: Vec<QueueConfig>,

    /// Optional device config space owned by the VMM. Some vhost-user device
    /// models, like virtio-fs, keep config-space data in the frontend instead
    /// of requiring backend protocol CONFIG support.
    config_space: Option<Vec<u8>>,

    /// Available features from the backend
    avail_features: u64,

    /// Whether the backend supports protocol features
    has_protocol_features: bool,

    /// Negotiated vhost-user protocol features.
    protocol_features: u64,

    /// Whether mutating requests should ask for an explicit status reply.
    reply_ack: bool,

    /// Optional guest ranges that the backend may map. External virtio-fs
    /// mounts only need guest RAM for vrings and request buffers; they must not
    /// receive pmem or DAX/shared-memory windows.
    shared_memory_ranges: Option<Vec<(u64, u64)>>,

    /// Acknowledged features
    acked_features: u64,

    /// Activated queue ownership. The vhost-user backend consumes the rings
    /// asynchronously, so snapshot state must be read back from the backend.
    queues: Option<Vec<DeviceQueue>>,

    /// Vring bases captured by GET_VRING_BASE while the backend is paused.
    paused_vring_bases: Option<Vec<u32>>,

    /// Restore activation should configure vrings but not enable them until
    /// `restore_state` applies the saved bases.
    restore_pending: bool,

    /// Device state
    device_state: DeviceState,

    /// Activation event (registered with EventManager)
    activate_evt: EventFd,

    /// Vring call event (backend->VMM interrupt notification)
    vring_call_event: Option<EventFd>,
}

impl VhostUserDevice {
    /// Create a new vhost-user device by connecting to a socket.
    ///
    /// # Arguments
    ///
    /// * `socket_path` - Path to the vhost-user Unix domain socket
    /// * `device_type` - Virtio device type ID
    /// * `device_name` - Human-readable device name for logging
    /// * `num_queues` - Number of queues (0 = query backend via MQ protocol)
    /// * `queue_sizes` - Size for each queue (empty = use default 256)
    ///
    /// # Returns
    ///
    /// A new VhostUserDevice or an error if connection fails.
    pub fn new(
        socket_path: impl AsRef<std::path::Path>,
        device_type: u32,
        device_name: String,
        num_queues: u16,
        queue_sizes: &[u16],
    ) -> IoResult<Self> {
        Self::with_config_space(
            socket_path,
            device_type,
            device_name,
            num_queues,
            queue_sizes,
            None,
        )
    }

    pub fn with_config_space(
        socket_path: impl AsRef<std::path::Path>,
        device_type: u32,
        device_name: String,
        num_queues: u16,
        queue_sizes: &[u16],
        config_space: Option<Vec<u8>>,
    ) -> IoResult<Self> {
        Self::with_config_space_and_memory_ranges(
            socket_path,
            device_type,
            device_name,
            num_queues,
            queue_sizes,
            config_space,
            None,
        )
    }

    pub fn with_config_space_and_memory_ranges(
        socket_path: impl AsRef<std::path::Path>,
        device_type: u32,
        device_name: String,
        num_queues: u16,
        queue_sizes: &[u16],
        config_space: Option<Vec<u8>>,
        shared_memory_ranges: Option<Vec<(u64, u64)>>,
    ) -> IoResult<Self> {
        debug!(
            "Connecting to vhost-user backend at {}",
            socket_path.as_ref().display()
        );

        // Connect to the vhost-user backend
        let stream = UnixStream::connect(socket_path)?;
        // NOTE: `num_queues` could be 0 here, but this is actually fine
        // because if `VhostUserProtocolFeatures::MQ` is supported the negotiated
        // value will be used automatically by Frontend
        let mut frontend = Frontend::from_stream(stream);

        // Get available features from backend
        let avail_features = frontend.get_features().map_err(io::Error::other)?;

        debug!("{}: backend features: 0x{:x}", device_name, avail_features);

        // Strip the vhost specific bit to leave only standard virtio features
        let has_protocol_features = avail_features & VHOST_USER_F_PROTOCOL_FEATURES != 0;
        let avail_features = avail_features & !VHOST_USER_F_PROTOCOL_FEATURES;

        let mut negotiated_protocol_features = 0;
        if has_protocol_features {
            let protocol_features = frontend.get_protocol_features()?;

            if protocol_features & VHOST_USER_PROTOCOL_F_CONFIG != 0 && config_space.is_none() {
                negotiated_protocol_features |= VHOST_USER_PROTOCOL_F_CONFIG;
            }
            if protocol_features & VHOST_USER_PROTOCOL_F_MQ != 0 {
                negotiated_protocol_features |= VHOST_USER_PROTOCOL_F_MQ;
            }
            if protocol_features & VHOST_USER_PROTOCOL_F_REPLY_ACK != 0 {
                negotiated_protocol_features |= VHOST_USER_PROTOCOL_F_REPLY_ACK;
            }
            if protocol_features & VHOST_USER_PROTOCOL_F_CONFIGURE_MEM_SLOTS != 0 {
                negotiated_protocol_features |= VHOST_USER_PROTOCOL_F_CONFIGURE_MEM_SLOTS;
            }

            frontend.set_protocol_features(negotiated_protocol_features)?;
        }
        let reply_ack = negotiated_protocol_features & VHOST_USER_PROTOCOL_F_REPLY_ACK != 0;

        // Determine actual queue count - may require protocol feature negotiation
        let actual_num_queues = if num_queues == 0 {
            if negotiated_protocol_features & VHOST_USER_PROTOCOL_F_MQ != 0 {
                let backend_queue_num = frontend.get_queue_num()?;

                debug!(
                    "{}: backend reports {} queues available",
                    device_name, backend_queue_num
                );

                backend_queue_num as usize
            } else {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "Backend doesn't support protocol features, must specify queue count",
                ));
            }
        } else {
            num_queues as usize
        };

        let default_size = queue_sizes.last().copied().unwrap_or(256);
        let queue_configs: Vec<_> = (0..actual_num_queues)
            .map(|i| {
                let size = queue_sizes.get(i).copied().unwrap_or(default_size);
                QueueConfig::new(size)
            })
            .collect();

        Ok(Self {
            frontend: Arc::new(Mutex::new(frontend)),
            device_type,
            device_name,
            queue_configs,
            config_space,
            avail_features,
            has_protocol_features,
            protocol_features: negotiated_protocol_features,
            reply_ack,
            shared_memory_ranges,
            acked_features: 0,
            queues: None,
            paused_vring_bases: None,
            restore_pending: false,
            device_state: DeviceState::Inactive,
            activate_evt: EventFd::new(EFD_NONBLOCK)?,
            vring_call_event: None,
        })
    }

    /// Activate the vhost-user device by setting up memory and vrings.
    fn activate_vhost_user(
        &mut self,
        mem: &GuestMemoryMmap,
        queues: &[DeviceQueue],
    ) -> IoResult<()> {
        let mut frontend = self.frontend.lock().unwrap();

        debug!("{}: activating vhost-user device", self.device_name);

        // Combine guest-acked features with backend-only features (QEMU approach)
        let backend_feature_bits = if self.has_protocol_features {
            self.acked_features | VHOST_USER_F_PROTOCOL_FEATURES
        } else {
            self.acked_features
        };

        frontend.set_owner(self.reply_ack)?;

        // Only share file-backed guest RAM with backends that opted into a
        // bounded memory table. This keeps pmem and DAX windows outside the
        // external vhost-user snapshot boundary.
        let regions: Vec<VhostUserMemoryRegionInfo> = mem
            .iter()
            .filter(|region| {
                let Some(ranges) = self.shared_memory_ranges.as_ref() else {
                    return true;
                };
                ranges.iter().any(|(addr, size)| {
                    region.start_addr().raw_value() == *addr && region.len() == *size
                })
            })
            .filter_map(|region| {
                if let Some(file_offset) = region.file_offset() {
                    Some(VhostUserMemoryRegionInfo {
                        guest_phys_addr: region.start_addr().raw_value(),
                        memory_size: region.len(),
                        userspace_addr: region.as_ptr() as u64,
                        mmap_offset: file_offset.start(),
                        mmap_handle: file_offset.file().as_raw_fd(),
                    })
                } else {
                    None
                }
            })
            .collect();
        if regions.is_empty() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "no file-backed guest RAM regions available for vhost-user backend",
            ));
        }

        debug!(
            "{}: sharing {} file-backed regions with backend",
            self.device_name,
            regions.len()
        );

        frontend
            .set_memory_regions(
                &regions,
                self.protocol_features & VHOST_USER_PROTOCOL_F_CONFIGURE_MEM_SLOTS != 0,
                self.reply_ack,
            )
            .map_err(|e| {
                error!("{}: set_mem_table failed: {:?}", self.device_name, e);
                io::Error::other(e)
            })?;

        // If protocol features not negotiated, this triggers automatic ring enabling
        if !self.restore_pending {
            frontend.set_features(backend_feature_bits, self.reply_ack)?;
        }

        let vring_call_event = EventFd::new(EFD_NONBLOCK)?;

        for (queue_index, device_queue) in queues.iter().enumerate() {
            let queue = &device_queue.queue;

            frontend.set_vring_num(queue_index, queue.actual_size(), self.reply_ack)?;

            // Set vring base
            frontend.set_vring_base(queue_index, 0, self.reply_ack)?;

            // Vring addresses in queue are GPAs, but vhost-user protocol expects VMM VAs
            let desc_table_gpa = queue.desc_table.0;
            let avail_ring_gpa = queue.avail_ring.0;
            let used_ring_gpa = queue.used_ring.0;

            let desc_table_vmm =
                mem.get_host_address(Address::new(desc_table_gpa))
                    .map_err(|_| {
                        io::Error::new(
                            ErrorKind::InvalidInput,
                            format!("GPA 0x{:x} not found in any memory region", desc_table_gpa),
                        )
                    })? as u64;
            let avail_ring_vmm =
                mem.get_host_address(Address::new(avail_ring_gpa))
                    .map_err(|_| {
                        io::Error::new(
                            ErrorKind::InvalidInput,
                            format!("GPA 0x{:x} not found in any memory region", avail_ring_gpa),
                        )
                    })? as u64;
            let used_ring_vmm = mem
                .get_host_address(Address::new(used_ring_gpa))
                .map_err(|_| {
                    io::Error::new(
                        ErrorKind::InvalidInput,
                        format!("GPA 0x{:x} not found in any memory region", used_ring_gpa),
                    )
                })? as u64;

            frontend
                .set_vring_addr(
                    queue_index,
                    desc_table_vmm,
                    used_ring_vmm,
                    avail_ring_vmm,
                    self.reply_ack,
                )
                .map_err(|e| {
                    error!("{}: set_vring_addr failed: {:?}", self.device_name, e);
                    io::Error::other(e)
                })?;

            frontend
                .set_vring_kick(queue_index, device_queue.event.as_raw_fd(), self.reply_ack)
                .map_err(|e| {
                    error!("{}: set_vring_kick failed: {:?}", self.device_name, e);
                    io::Error::other(e)
                })?;

            frontend
                .set_vring_call(
                    queue_index,
                    eventfd_write_fd(&vring_call_event),
                    self.reply_ack,
                )
                .map_err(|e| {
                    error!("{}: set_vring_call failed: {:?}", self.device_name, e);
                    io::Error::other(e)
                })?;

            // Per QEMU vhost.c: when VHOST_USER_F_PROTOCOL_FEATURES is not negotiated,
            // the rings start directly in the enabled state, and set_vring_enable will fail.
            if self.has_protocol_features && !self.restore_pending {
                frontend.set_vring_enable(queue_index, true, self.reply_ack)?;
            } else if !self.has_protocol_features {
                debug!(
                    "{}: vring {} already enabled (protocol features not negotiated)",
                    self.device_name, queue_index
                );
            }
        }

        self.vring_call_event = Some(vring_call_event);

        debug!(
            "{}: vhost-user device activated successfully",
            self.device_name
        );

        Ok(())
    }
}

impl VirtioDevice for VhostUserDevice {
    fn device_type(&self) -> u32 {
        self.device_type
    }

    fn device_name(&self) -> &str {
        &self.device_name
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &self.queue_configs
    }

    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features;
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        if let Some(config_space) = &self.config_space {
            let offset = offset as usize;
            data.fill(0);
            if offset < config_space.len() {
                let n = data.len().min(config_space.len() - offset);
                data[..n].copy_from_slice(&config_space[offset..offset + n]);
            }
            return;
        }

        // Fetch config from backend on every read (same as QEMU/crosvm)
        // No caching to avoid invalidation issues
        if self.protocol_features & VHOST_USER_PROTOCOL_F_CONFIG != 0
            && let Ok(mut frontend) = self.frontend.lock()
        {
            match frontend.get_config(offset as u32, data.len() as u32, 0) {
                Ok(returned_buf) => {
                    if data.len() <= returned_buf.len() {
                        data.copy_from_slice(&returned_buf[..data.len()]);
                        debug!(
                            "{}: read {} bytes from config at offset {}",
                            self.device_name,
                            data.len(),
                            offset
                        );
                        return;
                    }
                }
                Err(e) => {
                    debug!(
                        "{}: failed to read config from backend: {:?}",
                        self.device_name, e
                    );
                }
            }
        }

        debug!(
            "{}: config read at offset {} returning zeros (backend not available)",
            self.device_name, offset
        );
        data.fill(0);
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        if let Some(config_space) = &mut self.config_space {
            let offset = offset as usize;
            if offset < config_space.len() {
                let n = data.len().min(config_space.len() - offset);
                config_space[offset..offset + n].copy_from_slice(&data[..n]);
            }
            return;
        }

        if self.protocol_features & VHOST_USER_PROTOCOL_F_CONFIG == 0 {
            debug!(
                "{}: config write at offset {} skipped (no config protocol feature)",
                self.device_name, offset
            );
            return;
        }

        if let Ok(mut frontend) = self.frontend.lock() {
            match frontend.set_config(offset as u32, 0, data, self.reply_ack) {
                Ok(_) => {
                    debug!(
                        "{}: wrote {} bytes to config at offset {}",
                        self.device_name,
                        data.len(),
                        offset
                    );
                }
                Err(e) => {
                    warn!(
                        "{}: failed to write config at offset {}: {:?}",
                        self.device_name, offset, e
                    );
                }
            }
        }
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        if self.restore_pending && !self.has_protocol_features {
            error!(
                "{}: restore requires protocol-feature backends so rings can stay disabled until state is applied",
                self.device_name
            );
            return Err(ActivateError::BadActivate);
        }
        if let Err(e) = self.activate_vhost_user(&mem, &queues) {
            error!(
                "{}: failed to activate vhost-user device: {}",
                self.device_name, e
            );
            return Err(ActivateError::BadActivate);
        }

        self.queues = Some(queues);
        self.device_state = DeviceState::Activated(mem, interrupt);

        if let Err(e) = self.activate_evt.write(1) {
            error!(
                "{}: failed to write activate event: {}",
                self.device_name, e
            );
            return Err(ActivateError::BadActivate);
        }

        Ok(())
    }

    fn is_activated(&self) -> bool {
        matches!(self.device_state, DeviceState::Activated(_, _))
    }

    fn reset(&mut self) -> bool {
        debug!("{}: resetting vhost-user device", self.device_name);

        // Disable all vrings
        if let Ok(mut frontend) = self.frontend.lock() {
            for queue_index in 0..self.queue_configs.len() {
                if let Err(e) = frontend.set_vring_enable(queue_index, false, self.reply_ack) {
                    debug!(
                        "{}: failed to disable vring {} during reset: {}",
                        self.device_name, queue_index, e
                    );
                }
            }
        }

        self.vring_call_event = None;
        self.queues = None;
        self.paused_vring_bases = None;
        self.restore_pending = false;
        self.device_state = DeviceState::Inactive;
        true
    }

    fn pause(&mut self) -> Result<(), DeviceSnapshotError> {
        if self.restore_pending || self.paused_vring_bases.is_some() {
            return Ok(());
        }
        if !self.has_protocol_features {
            return Err(DeviceSnapshotError::Refused(format!(
                "{} requires vhost-user protocol features for snapshot-safe pause",
                self.device_name
            )));
        }
        let queue_count = self
            .queues
            .as_ref()
            .ok_or_else(|| {
                DeviceSnapshotError::Invalid(format!(
                    "{} pause before activation",
                    self.device_name
                ))
            })?
            .len();
        let mut frontend = self.frontend.lock().unwrap();
        let mut bases = Vec::with_capacity(queue_count);
        for queue_index in 0..queue_count {
            match frontend.get_vring_base(queue_index) {
                Ok(base) => bases.push(base),
                Err(e) => {
                    if let (Some(queues), Some(call_event)) =
                        (self.queues.as_ref(), self.vring_call_event.as_ref())
                    {
                        for (resume_index, base) in bases.into_iter().enumerate() {
                            let _ = Self::resume_vring(
                                &mut frontend,
                                resume_index,
                                base,
                                &queues[resume_index],
                                call_event,
                                self.reply_ack,
                            );
                        }
                    }
                    return Err(DeviceSnapshotError::Refused(format!(
                        "{} could not pause vring {queue_index}: {e}",
                        self.device_name
                    )));
                }
            }
        }
        self.paused_vring_bases = Some(bases);
        Ok(())
    }

    fn resume(&mut self) -> Result<(), DeviceSnapshotError> {
        let Some(bases) = self.paused_vring_bases.take() else {
            return Ok(());
        };
        let queues = self.queues.as_ref().ok_or_else(|| {
            DeviceSnapshotError::Invalid(format!("{} resume before activation", self.device_name))
        })?;
        let call_event = self.vring_call_event.as_ref().ok_or_else(|| {
            DeviceSnapshotError::Invalid(format!("{} resume missing call event", self.device_name))
        })?;
        if bases.len() != queues.len() {
            return Err(DeviceSnapshotError::Invalid(format!(
                "{} expected {} paused bases, got {}",
                self.device_name,
                queues.len(),
                bases.len()
            )));
        }
        let mut frontend = self.frontend.lock().unwrap();
        for (queue_index, base) in bases.into_iter().enumerate() {
            Self::resume_vring(
                &mut frontend,
                queue_index,
                base,
                &queues[queue_index],
                call_event,
                self.reply_ack,
            )
            .map_err(|e| {
                DeviceSnapshotError::Invalid(format!(
                    "{} could not resume vring {queue_index}: {e}",
                    self.device_name
                ))
            })?;
        }
        Ok(())
    }

    fn prepare_restore_activation(&mut self) {
        self.restore_pending = true;
    }

    fn serialize_state(&self) -> Result<DeviceSnapshot, DeviceSnapshotError> {
        let queues = self.queues.as_ref().ok_or_else(|| {
            DeviceSnapshotError::Invalid(format!(
                "{} serialize before activation",
                self.device_name
            ))
        })?;
        let vring_bases = self.paused_vring_bases.as_ref().ok_or_else(|| {
            DeviceSnapshotError::Invalid(format!("{} serialize before pause", self.device_name))
        })?;
        if queues.len() != vring_bases.len() {
            return Err(DeviceSnapshotError::Invalid(format!(
                "{} queue count changed during pause",
                self.device_name
            )));
        }
        let queue_states = queues
            .iter()
            .zip(vring_bases)
            .map(|(queue, base)| {
                let mut state = queue.queue.to_state();
                state.next_avail = *base as u16;
                state
            })
            .collect();
        let body = VhostUserSnapshotBody {
            device_type: self.device_type,
            acked_features: self.acked_features,
            config_space: self.config_space.clone(),
            vring_bases: vring_bases.clone(),
        };
        let payload =
            bincode::serialize(&body).map_err(|e| DeviceSnapshotError::Codec(e.to_string()))?;
        Ok(DeviceSnapshot {
            queues: queue_states,
            payload,
        })
    }

    fn restore_state(&mut self, snap: &DeviceSnapshot) -> Result<(), DeviceSnapshotError> {
        let queues = self.queues.as_mut().ok_or_else(|| {
            DeviceSnapshotError::Invalid(format!("{} restore before activation", self.device_name))
        })?;
        if snap.queues.len() != queues.len() {
            return Err(DeviceSnapshotError::Invalid(format!(
                "{} expected {} queues, got {}",
                self.device_name,
                queues.len(),
                snap.queues.len()
            )));
        }
        let body: VhostUserSnapshotBody = bincode::deserialize(&snap.payload)
            .map_err(|e| DeviceSnapshotError::Codec(e.to_string()))?;
        if body.device_type != self.device_type || body.config_space != self.config_space {
            return Err(DeviceSnapshotError::Invalid(format!(
                "{} configuration mismatch",
                self.device_name
            )));
        }
        if body.vring_bases.len() != queues.len() {
            return Err(DeviceSnapshotError::Invalid(format!(
                "{} expected {} vring bases, got {}",
                self.device_name,
                queues.len(),
                body.vring_bases.len()
            )));
        }

        self.acked_features = body.acked_features;
        let backend_feature_bits = if self.has_protocol_features {
            self.acked_features | VHOST_USER_F_PROTOCOL_FEATURES
        } else {
            self.acked_features
        };
        let mut frontend = self.frontend.lock().unwrap();
        frontend
            .set_features(backend_feature_bits, self.reply_ack)
            .map_err(|e| {
                DeviceSnapshotError::Invalid(format!(
                    "{} could not restore features: {e}",
                    self.device_name
                ))
            })?;

        for (queue, state) in queues.iter_mut().zip(&snap.queues) {
            queue.queue.restore_state(state);
        }
        for (queue_index, base) in body.vring_bases.into_iter().enumerate() {
            frontend
                .set_vring_base(queue_index, base, self.reply_ack)
                .map_err(|e| {
                    DeviceSnapshotError::Invalid(format!(
                        "{} could not restore vring {queue_index} base: {e}",
                        self.device_name
                    ))
                })?;
            frontend
                .set_vring_enable(queue_index, true, self.reply_ack)
                .map_err(|e| {
                    DeviceSnapshotError::Invalid(format!(
                        "{} could not enable restored vring {queue_index}: {e}",
                        self.device_name
                    ))
                })?;
        }
        self.restore_pending = false;
        self.paused_vring_bases = None;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct VhostUserSnapshotBody {
    device_type: u32,
    acked_features: u64,
    config_space: Option<Vec<u8>>,
    vring_bases: Vec<u32>,
}

impl VhostUserDevice {
    fn resume_vring(
        frontend: &mut Frontend,
        queue_index: usize,
        base: u32,
        queue: &DeviceQueue,
        call_event: &EventFd,
        reply_ack: bool,
    ) -> IoResult<()> {
        frontend.set_vring_base(queue_index, base, reply_ack)?;
        frontend.set_vring_kick(queue_index, queue.event.as_raw_fd(), reply_ack)?;
        frontend.set_vring_call(queue_index, eventfd_write_fd(call_event), reply_ack)?;
        frontend.set_vring_enable(queue_index, true, reply_ack)?;
        Ok(())
    }

    fn handle_vring_call_event(&mut self, event: &EpollEvent) {
        debug!("{}: vring call event received", self.device_name);

        let event_set = event.event_set();
        if event_set != EventSet::IN {
            warn!(
                "{}: vring call unexpected event {event_set:?}",
                self.device_name
            );
            return;
        }

        if let Some(ref vring_call_event) = self.vring_call_event {
            if let Err(e) = vring_call_event.read() {
                error!(
                    "{}: failed to read vring_call_event: {}",
                    self.device_name, e
                );
                return;
            }
        } else {
            error!("{}: vring_call_event is None", self.device_name);
            return;
        }

        if let DeviceState::Activated(_, ref interrupt) = self.device_state {
            debug!(
                "{}: interrupt received from backend, signaling guest",
                self.device_name
            );
            interrupt.signal_used_queue();
        }
    }

    fn handle_activate_event(&mut self, event_manager: &mut EventManager) {
        debug!("{}: activate event", self.device_name);

        if let Err(e) = self.activate_evt.read() {
            error!(
                "{}: failed to consume activate event: {}",
                self.device_name, e
            );
        }

        if let Some(ref vring_call_event) = self.vring_call_event {
            let self_subscriber = event_manager
                .subscriber(self.activate_evt.as_raw_fd())
                .unwrap();

            event_manager
                .register(
                    vring_call_event.as_raw_fd(),
                    EpollEvent::new(EventSet::IN, vring_call_event.as_raw_fd() as u64),
                    self_subscriber.clone(),
                )
                .unwrap_or_else(|e| {
                    error!(
                        "{}: failed to register vring_call_event with event manager: {e:?}",
                        self.device_name
                    );
                });
        } else {
            error!(
                "{}: vring_call_event is None during activation",
                self.device_name
            );
        }

        // Unregister activate_evt as it's only needed once
        event_manager
            .unregister(self.activate_evt.as_raw_fd())
            .unwrap_or_else(|e| {
                error!(
                    "{}: failed to unregister activate event: {e:?}",
                    self.device_name
                );
            });
    }
}

impl Subscriber for VhostUserDevice {
    fn process(&mut self, event: &EpollEvent, event_manager: &mut EventManager) {
        let source = event.fd();
        let activate_evt_fd = self.activate_evt.as_raw_fd();
        let vring_call_fd = self
            .vring_call_event
            .as_ref()
            .map(|e| e.as_raw_fd())
            .unwrap_or(-1);

        if self.is_activated() {
            match source {
                _ if source == vring_call_fd => self.handle_vring_call_event(event),
                _ if source == activate_evt_fd => self.handle_activate_event(event_manager),
                _ => warn!(
                    "{}: unexpected event received: {source:?}",
                    self.device_name
                ),
            }
        } else if source == activate_evt_fd {
            // Allow activation event even before device is activated
            self.handle_activate_event(event_manager);
        } else {
            warn!(
                "{}: device not yet activated, spurious event received: {source:?}",
                self.device_name
            );
        }
    }

    fn interest_list(&self) -> Vec<EpollEvent> {
        vec![EpollEvent::new(
            EventSet::IN,
            self.activate_evt.as_raw_fd() as u64,
        )]
    }
}
