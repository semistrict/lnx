//! vmnet.framework backing for routable per-VM networking on macOS.
//!
//! The ingress daemon reserves one NAT-mode vmnet network with a dedicated
//! subnet and the vmnet DHCP server disabled; lnx allocates guest addresses
//! itself and the guests configure them statically. Each VM gets its own
//! interface on that network, pumped to a datagram socketpair whose far end
//! is handed to the VM owner process and attached to the virtio-net device.
//!
//! Creating vmnet interfaces requires root (or the restricted
//! `com.apple.vm.networking` entitlement), which is why this lives behind
//! the ingress daemon rather than in the per-VM owner processes.

use std::ffi::{c_char, c_int, c_void};
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use block2::{Block, RcBlock};

type XpcObject = *mut c_void;
type DispatchQueue = *mut c_void;
type InterfaceRef = *mut c_void;
type NetworkConfigurationRef = *mut c_void;
type NetworkRef = *mut c_void;

const VMNET_SUCCESS: u32 = 1000;
const VMNET_SHARED_MODE: u32 = 1001;
const VMNET_INTERFACE_PACKETS_AVAILABLE: u32 = 1 << 0;

const READ_BATCH: usize = 32;
/// Largest frame the libkrun unixgram socket can carry (MAX_BUFFER_SIZE minus
/// the virtio-net header it strips). Sizing reads to this avoids truncating a
/// guest frame into a corrupt packet on the segment.
const MAX_FRAME_SIZE: usize = 65550;
const RECV_TIMEOUT_MS: i64 = 250;

#[repr(C)]
struct VmPktDesc {
    vm_pkt_size: usize,
    vm_pkt_iov: *mut libc::iovec,
    vm_pkt_iovcnt: u32,
    vm_flags: u32,
}

unsafe extern "C" {
    fn xpc_dictionary_create(
        keys: *const *const c_char,
        values: *const XpcObject,
        count: usize,
    ) -> XpcObject;
    fn xpc_dictionary_set_bool(dict: XpcObject, key: *const c_char, value: bool);
    fn xpc_dictionary_get_uint64(dict: XpcObject, key: *const c_char) -> u64;
    fn xpc_release(object: XpcObject);

    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueue;
    fn dispatch_release(object: DispatchQueue);
}

#[link(name = "vmnet", kind = "framework")]
unsafe extern "C" {
    static vmnet_allocate_mac_address_key: *const c_char;
    static vmnet_max_packet_size_key: *const c_char;

    fn vmnet_network_configuration_create(mode: u32, status: *mut u32) -> NetworkConfigurationRef;
    fn vmnet_network_configuration_set_ipv4_subnet(
        config: NetworkConfigurationRef,
        subnet_addr: *const libc::in_addr,
        subnet_mask: *const libc::in_addr,
    ) -> u32;
    fn vmnet_network_configuration_disable_dhcp(config: NetworkConfigurationRef);
    fn vmnet_network_create(config: NetworkConfigurationRef, status: *mut u32) -> NetworkRef;
    fn vmnet_interface_start_with_network(
        network: NetworkRef,
        interface_desc: XpcObject,
        queue: DispatchQueue,
        handler: &Block<dyn Fn(u32, XpcObject)>,
    ) -> InterfaceRef;
    fn vmnet_interface_set_event_callback(
        interface: InterfaceRef,
        event_mask: u32,
        queue: DispatchQueue,
        callback: *const c_void,
    ) -> u32;
    fn vmnet_stop_interface(
        interface: InterfaceRef,
        queue: DispatchQueue,
        handler: &Block<dyn Fn(u32)>,
    ) -> u32;
    fn vmnet_read(interface: InterfaceRef, packets: *mut VmPktDesc, pktcnt: *mut c_int) -> u32;
    fn vmnet_write(interface: InterfaceRef, packets: *mut VmPktDesc, pktcnt: *mut c_int) -> u32;
}

fn vmnet_error(call: &str, status: u32) -> anyhow::Error {
    let name = match status {
        1001 => "VMNET_FAILURE",
        1002 => "VMNET_MEM_FAILURE",
        1003 => "VMNET_INVALID_ARGUMENT",
        1004 => "VMNET_SETUP_INCOMPLETE",
        1005 => "VMNET_INVALID_ACCESS",
        1006 => "VMNET_PACKET_TOO_BIG",
        1007 => "VMNET_BUFFER_EXHAUSTED",
        1008 => "VMNET_TOO_MANY_PACKETS",
        1009 => "VMNET_SHARING_SERVICE_BUSY",
        1010 => "VMNET_NOT_AUTHORIZED",
        _ => "unknown",
    };
    anyhow::anyhow!("{call} failed: {name} ({status})")
}

/// Raw pointers used from dispatch callbacks and pump threads. The referents
/// are managed by vmnet/libdispatch and stay valid until the interface stops.
#[derive(Clone, Copy)]
struct SendPtr(*mut c_void);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

pub fn parse_subnet(spec: &str) -> Result<(Ipv4Addr, u8)> {
    let (addr, prefix) = spec
        .split_once('/')
        .with_context(|| format!("subnet {spec:?} must look like 192.168.106.0/24"))?;
    let addr: Ipv4Addr = addr
        .parse()
        .with_context(|| format!("invalid subnet address in {spec:?}"))?;
    let prefix: u8 = prefix
        .parse()
        .with_context(|| format!("invalid prefix length in {spec:?}"))?;
    // Guests need room for the reserved network/host/broadcast addresses.
    if !(8..=29).contains(&prefix) {
        bail!("subnet prefix /{prefix} out of range (8-29)");
    }
    let mask = mask_for_prefix(prefix);
    if u32::from(addr) & !u32::from(mask) != 0 {
        bail!("{spec} has host bits set; expected the network address");
    }
    Ok((addr, prefix))
}

pub fn mask_for_prefix(prefix: u8) -> Ipv4Addr {
    Ipv4Addr::from(u32::MAX << (32 - u32::from(prefix)))
}

/// The second address of the range is reserved for the host and acts as the
/// NAT gateway and DNS proxy for the guests.
pub fn gateway_for_subnet(subnet: Ipv4Addr) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(subnet) + 1)
}

pub struct Network {
    network: SendPtr,
    subnet: Ipv4Addr,
    prefix: u8,
}

impl Network {
    /// Reserves a NAT-mode vmnet network on the given subnet with the vmnet
    /// DHCP server disabled. The reservation lives until the process exits;
    /// the underlying objects are intentionally never released.
    pub fn create(subnet: Ipv4Addr, prefix: u8) -> Result<Network> {
        let mask = mask_for_prefix(prefix);
        // vmnet's set_ipv4_subnet takes the host (gateway) address, not the
        // network address: it assigns this to the host-side bridge. Passing
        // the network address (.0) would strand the guests, whose gateway is
        // .1. (Matches Apple's container tool.)
        let gateway_addr = libc::in_addr {
            s_addr: u32::from(gateway_for_subnet(subnet)).to_be(),
        };
        let mask_addr = libc::in_addr {
            s_addr: u32::from(mask).to_be(),
        };
        unsafe {
            let mut status: u32 = VMNET_SUCCESS;
            let config = vmnet_network_configuration_create(VMNET_SHARED_MODE, &mut status);
            if config.is_null() {
                return Err(vmnet_error("vmnet_network_configuration_create", status));
            }
            let rc = vmnet_network_configuration_set_ipv4_subnet(config, &gateway_addr, &mask_addr);
            if rc != VMNET_SUCCESS {
                return Err(vmnet_error(
                    "vmnet_network_configuration_set_ipv4_subnet",
                    rc,
                ));
            }
            vmnet_network_configuration_disable_dhcp(config);
            let mut status: u32 = VMNET_SUCCESS;
            let network = vmnet_network_create(config, &mut status);
            if network.is_null() {
                return Err(vmnet_error("vmnet_network_create", status));
            }
            Ok(Network {
                network: SendPtr(network),
                subnet,
                prefix,
            })
        }
    }

    pub fn subnet(&self) -> Ipv4Addr {
        self.subnet
    }

    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    pub fn gateway(&self) -> Ipv4Addr {
        gateway_for_subnet(self.subnet)
    }

    /// Starts a new interface on the network and pumps its frames to a
    /// datagram socketpair. The returned guest end is handed to the VM owner
    /// and attached to the virtio-net device; the guest brings its own MAC,
    /// so no vmnet-allocated address is requested.
    pub fn attach(&self, label: &str) -> Result<Attachment> {
        let (host_fd, guest_fd) = dgram_socketpair()?;

        let queue_label = std::ffi::CString::new(format!("lnx.vmnet.{label}"))
            .unwrap_or_else(|_| std::ffi::CString::new("lnx.vmnet").expect("static label"));
        let queue = unsafe { dispatch_queue_create(queue_label.as_ptr(), std::ptr::null()) };
        if queue.is_null() {
            bail!("dispatch_queue_create failed");
        }

        let (started_tx, started_rx) = mpsc::channel::<(u32, u64)>();
        let start_handler = RcBlock::new(move |status: u32, params: XpcObject| {
            let max_packet_size = if params.is_null() {
                0
            } else {
                unsafe { xpc_dictionary_get_uint64(params, vmnet_max_packet_size_key) }
            };
            let _ = started_tx.send((status, max_packet_size));
        });

        // Release the dispatch queue on any early return; success hands it to
        // the Attachment, which releases it on Drop.
        let queue_guard = QueueGuard(Some(queue));

        let interface = unsafe {
            let desc = xpc_dictionary_create(std::ptr::null(), std::ptr::null(), 0);
            xpc_dictionary_set_bool(desc, vmnet_allocate_mac_address_key, false);
            let interface =
                vmnet_interface_start_with_network(self.network.0, desc, queue, &start_handler);
            xpc_release(desc);
            interface
        };
        if interface.is_null() {
            bail!("vmnet_interface_start_with_network failed");
        }
        let interface = SendPtr(interface);
        // From here on the interface is started; tear it down on any error.
        let started = StartedInterface {
            interface,
            queue: queue_guard,
        };
        let (status, max_packet_size) = started_rx
            .recv_timeout(Duration::from_secs(10))
            .context("vmnet interface start timed out")?;
        if status != VMNET_SUCCESS {
            return Err(vmnet_error("vmnet_interface_start_with_network", status));
        }
        let max_packet_size = usize::try_from(max_packet_size.max(1514)).expect("packet size");

        let inner = Arc::new(AttachmentInner {
            interface,
            queue: SendPtr(queue),
            host_fd,
            stopped: AtomicBool::new(false),
            rx: Mutex::new(RxScratch::new(max_packet_size)),
            rx_frames: std::sync::atomic::AtomicU64::new(0),
            tx_frames: std::sync::atomic::AtomicU64::new(0),
        });

        // guest -> vmnet: a reader on the host end of the pair. The fd has a
        // recv timeout so the thread observes `stopped` even though a closed
        // datagram peer delivers no EOF. Spawn it before arming the vmnet
        // callback so this is the last fallible step.
        let write_inner = Arc::clone(&inner);
        let pump = thread::Builder::new()
            .name(format!("vmnet-{label}"))
            .spawn(move || write_inner.pump_guest_to_vmnet())
            .context("spawn vmnet pump thread")?;

        // vmnet -> guest: drain available packets from the dispatch callback
        // and forward each one as a single datagram. A full guest receive
        // buffer drops the frame (MSG_DONTWAIT), like a real link would.
        let read_inner = Arc::clone(&inner);
        let event_callback = RcBlock::new(move |_events: u32, _event: XpcObject| {
            read_inner.forward_available_packets();
        });
        let rc = unsafe {
            vmnet_interface_set_event_callback(
                interface.0,
                VMNET_INTERFACE_PACKETS_AVAILABLE,
                queue,
                &*event_callback as *const Block<dyn Fn(u32, XpcObject)> as *const c_void,
            )
        };
        if rc != VMNET_SUCCESS {
            // Pump is already running; signal it and let `started` Drop stop
            // the interface.
            inner.stop();
            let _ = pump.join();
            return Err(vmnet_error("vmnet_interface_set_event_callback", rc));
        }

        // vmnet copies the callback block, so our handle can drop here; the
        // copy stays alive until the callback is disabled at teardown.
        drop(event_callback);

        // No fallible steps remain: take over teardown from the start guard.
        started.into_owned();
        Ok(Attachment {
            guest_fd: Some(guest_fd),
            inner,
            pump: Some(pump),
        })
    }
}

/// Releases a dispatch queue on Drop unless ownership is handed off.
struct QueueGuard(Option<DispatchQueue>);

impl Drop for QueueGuard {
    fn drop(&mut self) {
        if let Some(queue) = self.0.take() {
            unsafe { dispatch_release(queue) };
        }
    }
}

unsafe impl Send for QueueGuard {}

/// Stops and releases a started interface (and its queue) on Drop, until
/// `into_owned` transfers that responsibility to the Attachment.
struct StartedInterface {
    interface: SendPtr,
    queue: QueueGuard,
}

impl StartedInterface {
    fn into_owned(mut self) {
        // The Attachment now owns teardown; forget the queue so Drop is a
        // no-op and leak nothing.
        self.queue.0.take();
        std::mem::forget(self);
    }
}

impl Drop for StartedInterface {
    fn drop(&mut self) {
        stop_interface(self.interface.0, self.queue.0.take());
    }
}

pub struct Attachment {
    guest_fd: Option<OwnedFd>,
    inner: Arc<AttachmentInner>,
    pump: Option<thread::JoinHandle<()>>,
}

impl Attachment {
    /// The datagram socket to hand to the VM owner. One ethernet frame per
    /// datagram, matching libkrun's unixgram virtio-net backend.
    pub fn take_guest_fd(&mut self) -> Option<OwnedFd> {
        self.guest_fd.take()
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.inner.stop();
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
        stop_interface(self.inner.interface.0, Some(self.inner.queue.0));
    }
}

/// Disables the event callback and stops the interface, draining the
/// completion handler so vmnet finishes teardown before the queue is freed.
fn stop_interface(interface: *mut c_void, queue: Option<DispatchQueue>) {
    unsafe {
        vmnet_interface_set_event_callback(interface, 0, std::ptr::null_mut(), std::ptr::null());
    }
    let (stopped_tx, stopped_rx) = mpsc::channel::<u32>();
    let handler = RcBlock::new(move |status: u32| {
        let _ = stopped_tx.send(status);
    });
    if let Some(queue) = queue {
        let rc = unsafe { vmnet_stop_interface(interface, queue, &handler) };
        if rc == VMNET_SUCCESS {
            let _ = stopped_rx.recv_timeout(Duration::from_secs(5));
        }
        unsafe { dispatch_release(queue) };
    }
}

/// Reusable receive buffers for the vmnet -> guest path. The dispatch queue
/// is serial, so a single scratch set behind a Mutex never contends; this
/// keeps the RX hot path allocation-free.
struct RxScratch {
    buffers: Vec<Vec<u8>>,
}

impl RxScratch {
    fn new(max_packet_size: usize) -> Self {
        Self {
            buffers: (0..READ_BATCH).map(|_| vec![0u8; max_packet_size]).collect(),
        }
    }
}

struct AttachmentInner {
    interface: SendPtr,
    queue: SendPtr,
    host_fd: OwnedFd,
    stopped: AtomicBool,
    rx: Mutex<RxScratch>,
    rx_frames: std::sync::atomic::AtomicU64,
    tx_frames: std::sync::atomic::AtomicU64,
}

unsafe impl Send for AttachmentInner {}
unsafe impl Sync for AttachmentInner {}

/// Set LNX_VMNET_DEBUG to log the first frames in each direction — useful for
/// diagnosing a guest that can't reach its gateway.
fn vmnet_debug() -> bool {
    std::env::var_os("LNX_VMNET_DEBUG").is_some()
}


impl AttachmentInner {
    fn forward_available_packets(&self) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        let mut rx = self.rx.lock().unwrap();
        let capacity = rx.buffers.first().map(|b| b.len()).unwrap_or(0);
        loop {
            // vmnet_read mutates each descriptor's vm_pkt_size/iov_len, so
            // rebuild the iovecs and descriptors at full capacity each pass.
            let mut iovs: Vec<libc::iovec> = rx
                .buffers
                .iter_mut()
                .map(|buffer| libc::iovec {
                    iov_base: buffer.as_mut_ptr().cast(),
                    iov_len: capacity,
                })
                .collect();
            let mut descs: Vec<VmPktDesc> = iovs
                .iter_mut()
                .map(|iov| VmPktDesc {
                    vm_pkt_size: capacity,
                    vm_pkt_iov: iov,
                    vm_pkt_iovcnt: 1,
                    vm_flags: 0,
                })
                .collect();
            let mut count = READ_BATCH as c_int;
            let rc = unsafe { vmnet_read(self.interface.0, descs.as_mut_ptr(), &mut count) };
            if rc != VMNET_SUCCESS || count <= 0 {
                return;
            }
            let sizes: Vec<usize> = descs.iter().take(count as usize).map(|d| d.vm_pkt_size).collect();
            if vmnet_debug() {
                let n = self.rx_frames.fetch_add(count as u64, Ordering::Relaxed);
                if n < 16 {
                    eprintln!("vmnet.rx frames+={count} total={} sizes={sizes:?}", n + count as u64);
                }
            }
            for (size, buffer) in sizes.iter().zip(&rx.buffers) {
                let frame = &buffer[..*size];
                // MSG_DONTWAIT: a full guest receive buffer drops the frame
                // (EWOULDBLOCK / ENOBUFS on macOS) instead of stalling this
                // serial dispatch queue.
                let _ = unsafe {
                    libc::send(
                        self.host_fd.as_raw_fd(),
                        frame.as_ptr().cast(),
                        frame.len(),
                        libc::MSG_DONTWAIT,
                    )
                };
            }
            if (count as usize) < READ_BATCH {
                return;
            }
        }
    }

    fn pump_guest_to_vmnet(&self) {
        // Sized for the largest frame the unixgram socket can carry so a
        // truncated read never injects a corrupt frame into the segment.
        let mut buffer = vec![0u8; MAX_FRAME_SIZE];
        loop {
            let received = unsafe {
                libc::recv(
                    self.host_fd.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    0,
                )
            };
            if received < 0 {
                let err = std::io::Error::last_os_error();
                match err.raw_os_error() {
                    // Recv timeout / interrupted: re-check the stop flag and
                    // keep going. The timeout is how the thread notices a
                    // closed datagram peer, which delivers no EOF.
                    Some(libc::EAGAIN) | Some(libc::EINTR) => {
                        if self.stopped.load(Ordering::SeqCst) {
                            return;
                        }
                        continue;
                    }
                    _ => return,
                }
            }
            if received == 0 {
                // A zero-length datagram is not EOF on SOCK_DGRAM; only stop
                // when asked to.
                if self.stopped.load(Ordering::SeqCst) {
                    return;
                }
                continue;
            }
            let mut iov = libc::iovec {
                iov_base: buffer.as_mut_ptr().cast(),
                iov_len: received as usize,
            };
            let mut desc = VmPktDesc {
                vm_pkt_size: received as usize,
                vm_pkt_iov: &mut iov,
                vm_pkt_iovcnt: 1,
                vm_flags: 0,
            };
            let mut count: c_int = 1;
            // A frame vmnet rejects (e.g. oversized) is dropped, not fatal.
            let rc = unsafe { vmnet_write(self.interface.0, &mut desc, &mut count) };
            if vmnet_debug() {
                let n = self.tx_frames.fetch_add(1, Ordering::Relaxed);
                if n < 16 {
                    eprintln!("vmnet.tx frame={received} rc={rc} count={count}");
                }
            }
        }
    }

    /// Signals the pump to exit; the actual interface teardown happens in
    /// `stop_interface` from the Attachment Drop.
    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        // Best-effort wake; the recv timeout guarantees progress regardless.
        unsafe { libc::shutdown(self.host_fd.as_raw_fd(), libc::SHUT_RDWR) };
    }
}

fn dgram_socketpair() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as c_int; 2];
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("create vmnet socketpair");
    }
    let host = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let guest = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    // On macOS the datagram send buffer sets the max frame size and gives no
    // queuing; the receiver's buffer is what queues. Size both generously in
    // both directions so frames are dropped on overflow, never blocked.
    for fd in [&host, &guest] {
        let size: c_int = 4 * 1024 * 1024;
        for option in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
            unsafe {
                libc::setsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    option,
                    (&raw const size).cast(),
                    std::mem::size_of::<c_int>() as libc::socklen_t,
                );
            }
        }
    }
    // The pump reads the host end with a timeout so it can observe the stop
    // flag; a closed datagram peer never delivers EOF to wake a blocking recv.
    let timeout = libc::timeval {
        tv_sec: RECV_TIMEOUT_MS / 1000,
        tv_usec: ((RECV_TIMEOUT_MS % 1000) * 1000) as _,
    };
    unsafe {
        libc::setsockopt(
            host.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&raw const timeout).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
    }
    Ok((host, guest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_subnet_specs() {
        let (subnet, prefix) = parse_subnet("192.168.106.0/24").expect("parse");
        assert_eq!(subnet, Ipv4Addr::new(192, 168, 106, 0));
        assert_eq!(prefix, 24);

        assert!(parse_subnet("192.168.106.0").is_err());
        assert!(parse_subnet("192.168.106.1/24").is_err());
        assert!(parse_subnet("192.168.106.0/31").is_err());
        assert!(parse_subnet("bogus/24").is_err());
    }

    #[test]
    fn computes_mask_and_gateway() {
        assert_eq!(mask_for_prefix(24), Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(mask_for_prefix(16), Ipv4Addr::new(255, 255, 0, 0));
        assert_eq!(
            gateway_for_subnet(Ipv4Addr::new(192, 168, 106, 0)),
            Ipv4Addr::new(192, 168, 106, 1)
        );
    }
}
