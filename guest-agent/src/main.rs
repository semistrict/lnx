use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_ulong, c_void};
use std::fmt;
use std::io::{Error, Read, Write};
use std::mem::size_of;
use std::net::{Ipv4Addr, Shutdown, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use std::{env, fs, thread};

use lnx_protocol::{MAX_MESSAGE_SIZE, Message, PROTOCOL_VERSION};
mod user;
use user::{EXEC_HOME, EXEC_USER, ensure_exec_user};

const AF_VSOCK: c_int = 40;
const AF_UNIX: c_int = 1;
const AF_UNSPEC: c_int = 0;
const AF_INET: c_int = 2;
const AF_NETLINK: c_int = 16;
const SOCK_STREAM: c_int = 1;
const SOCK_RAW: c_int = 3;
const SOL_SOCKET: c_int = 1;
const SO_RCVTIMEO: c_int = 20;
const NETLINK_ROUTE: c_int = 0;
const VMADDR_CID_HOST: u32 = 2;
const AGENT_PORT: u32 = 10240;
const SNAPSHOT_PORT: u32 = 10241;
const EINTR: c_int = 4;
const EAGAIN: c_int = 11;
const ECHILD: c_int = 10;
const EIO: c_int = 5;
const EINVAL: c_int = 22;
const ENOENT: c_int = 2;
const ENOTTY: c_int = 25;
const EEXIST: c_int = 17;
const F_GETFD: c_int = 1;
const F_SETFD: c_int = 2;
const F_GETFL: c_int = 3;
const F_SETFL: c_int = 4;
const FD_CLOEXEC: c_int = 1;
const O_NONBLOCK: c_int = 0o4000;
const STDIN_FILENO: c_int = 0;
const STDOUT_FILENO: c_int = 1;
const STDERR_FILENO: c_int = 2;
const SIGTERM: c_int = 15;
const WNOHANG: c_int = 1;
const POLLIN: i16 = 0x0001;
const POLLERR: i16 = 0x0008;
const POLLHUP: i16 = 0x0010;
const POLLNVAL: i16 = 0x0020;
const TIOCSCTTY: c_ulong = 0x540e;
const TIOCSWINSZ: c_ulong = 0x5414;
const IFF_UP: c_uint = 0x1;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 0x0001;
const NLM_F_ACK: u16 = 0x0004;
const NLM_F_REPLACE: u16 = 0x0100;
const NLM_F_CREATE: u16 = 0x0400;
const RTM_NEWLINK: u16 = 16;
const RTM_NEWADDR: u16 = 20;
const RTM_NEWROUTE: u16 = 24;
const RT_SCOPE_UNIVERSE: u8 = 0;
const RT_TABLE_MAIN: u8 = 254;
const RTPROT_BOOT: u8 = 3;
const RTN_UNICAST: u8 = 1;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
// From Linux <linux/random.h>: _IOW('R', 0x03, int[2]) and _IO('R', 0x07).
const RNDADDENTROPY: c_ulong = 0x4008_5203;
const RNDRESEEDCRNG: c_ulong = 0x5207;
const CLOCK_REALTIME: c_int = 0;
const MS_RDONLY: c_ulong = 1;
const MS_REMOUNT: c_ulong = 32;
const MS_BIND: c_ulong = 4096;
const MS_REC: c_ulong = 16384;
const MS_PRIVATE: c_ulong = 262144;
const MNT_DETACH: c_int = 2;
const SYS_PIVOT_ROOT: isize = 41;
const FRAME_SNAPSHOT: u8 = b'K';
const FRAME_CONTROL_SNAPSHOT_EXIT: u8 = b'X';
const FRAME_CONTROL_OPEN_URL: u8 = b'O';
const FRAME_CONTROL_OK: u8 = b'x';
const MAX_CONTROL_PAYLOAD: usize = 16 * 1024;
const DEFAULT_PATH: &str = "/home/lnxuser/.local/bin:/home/lnxuser/go/bin:/home/lnxuser/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/snap/bin";
const AGENT_PATH: &str = "/run/lnx/lnx-agent";
const LNXCTL_PATH: &str = "/run/lnx/lnxctl";
const OLD_AGENT_PATH: &str = "/usr/local/lib/lnx/lnx-agent";
const OLD_LNXCTL_PATH: &str = "/usr/local/bin/lnxctl";
const XDG_OPEN_PATH: &str = "/usr/local/bin/xdg-open";
const DEFAULT_BROWSER: &str = XDG_OPEN_PATH;
const OLD_SERVICE_PATH: &str = "/etc/systemd/system/lnx-agent.service";
const OLD_WANTS_LINK: &str = "/etc/systemd/system/multi-user.target.wants/lnx-agent.service";
const CONTROL_SOCKET: &str = "/run/lnx-agent.sock";
const CONTROL_SOCKET_ENV: &str = "LNX_CONTROL_SOCKET";
const VIRTIOFS_DAX_ENV: &str = "LNX_VIRTIOFS_DAX";
const VMSTATE_RESEED_MARKER: &str = "/run/lnx-vmstate-reseed";
const SERVICE_PATH: &str = "/run/systemd/system/lnx-agent.service";
const WANTS_DIR: &str = "/run/systemd/system/multi-user.target.wants";
const WANTS_LINK: &str = "/run/systemd/system/multi-user.target.wants/lnx-agent.service";
const WANTS_LINK_C: &[u8] =
    b"/newroot/run/systemd/system/multi-user.target.wants/lnx-agent.service\0";
const SNAPSHOT_RESUME_READ_TIMEOUT_USECS: i64 = 500_000;
const RESTORE_ENTROPY_MIN_BYTES: usize = 32;
const RESTORE_ENTROPY_MAX_BYTES: usize = 1024;
const LISTENER_MONITOR_INTERVAL_MS: u64 = 1000;

enum ChannelInput {
    Data(Vec<u8>),
    Eof,
    Resize(u16, u16),
    Close,
    SnapshotComplete,
    SnapshotFailed,
    OpenUrlComplete,
    OpenUrlFailed,
}

enum ChannelControlRequest {
    SnapshotExit(c_int),
    OpenUrl { fd: c_int, url: String },
}

struct ChannelState {
    tx: mpsc::Sender<ChannelInput>,
    eof_requested: Arc<AtomicBool>,
}

#[repr(C)]
struct Sockaddr {
    sa_family: u16,
    sa_data: [u8; 14],
}

#[repr(C)]
struct SockaddrVm {
    svm_family: u16,
    svm_reserved1: u16,
    svm_port: u32,
    svm_cid: u32,
    svm_flags: u8,
    svm_zero: [u8; 3],
}

#[repr(C)]
struct SockaddrUn {
    sun_family: u16,
    sun_path: [u8; 108],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SockaddrNl {
    nl_family: u16,
    nl_pad: u16,
    nl_pid: u32,
    nl_groups: u32,
}

#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[repr(C)]
struct Timeval {
    tv_sec: i64,
    tv_usec: i64,
}

#[repr(C)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NlMsgHdr {
    nlmsg_len: u32,
    nlmsg_type: u16,
    nlmsg_flags: u16,
    nlmsg_seq: u32,
    nlmsg_pid: u32,
}

#[repr(C)]
struct IfInfoMsg {
    ifi_family: u8,
    ifi_pad: u8,
    ifi_type: u16,
    ifi_index: c_int,
    ifi_flags: c_uint,
    ifi_change: c_uint,
}

#[repr(C)]
struct IfAddrMsg {
    ifa_family: u8,
    ifa_prefixlen: u8,
    ifa_flags: u8,
    ifa_scope: u8,
    ifa_index: c_uint,
}

#[repr(C)]
struct RtMsg {
    rtm_family: u8,
    rtm_dst_len: u8,
    rtm_src_len: u8,
    rtm_tos: u8,
    rtm_table: u8,
    rtm_protocol: u8,
    rtm_scope: u8,
    rtm_type: u8,
    rtm_flags: c_uint,
}

#[repr(C)]
struct RtAttr {
    rta_len: u16,
    rta_type: u16,
}

#[repr(C)]
struct RandPoolInfo {
    entropy_count: c_int,
    buf_size: c_int,
    // The kernel treats this as a flexible byte payload after the two ints.
    buf: [u8; RESTORE_ENTROPY_MAX_BYTES],
}

unsafe extern "C" {
    fn accept(fd: c_int, addr: *mut Sockaddr, len: *mut c_uint) -> c_int;
    fn bind(fd: c_int, addr: *const Sockaddr, len: c_uint) -> c_int;
    fn chdir(path: *const c_char) -> c_int;
    fn clock_settime(clockid: c_int, tp: *const Timespec) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn connect(fd: c_int, addr: *const Sockaddr, len: c_uint) -> c_int;
    fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
    fn execve(file: *const c_char, argv: *const *const c_char, envp: *const *const c_char)
    -> c_int;
    fn execvp(file: *const c_char, argv: *const *const c_char) -> c_int;
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    fn fork() -> c_int;
    fn if_nametoindex(ifname: *const c_char) -> c_uint;
    fn listen(fd: c_int, backlog: c_int) -> c_int;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn kill(pid: c_int, sig: c_int) -> c_int;
    fn openpty(
        amaster: *mut c_int,
        aslave: *mut c_int,
        name: *mut c_char,
        termp: *const c_void,
        winp: *const c_void,
    ) -> c_int;
    fn poll(fds: *mut PollFd, nfds: usize, timeout: c_int) -> c_int;
    fn mount(
        source: *const c_char,
        target: *const c_char,
        filesystemtype: *const c_char,
        mountflags: c_ulong,
        data: *const c_void,
    ) -> c_int;
    fn pipe(fds: *mut c_int) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    fn socket(domain: c_int, typ: c_int, protocol: c_int) -> c_int;
    fn setsockopt(
        fd: c_int,
        level: c_int,
        optname: c_int,
        optval: *const c_void,
        optlen: c_uint,
    ) -> c_int;
    fn setsid() -> c_int;
    fn setenv(name: *const c_char, value: *const c_char, overwrite: c_int) -> c_int;
    fn setgid(gid: c_uint) -> c_int;
    fn setuid(uid: c_uint) -> c_int;
    fn symlink(target: *const c_char, linkpath: *const c_char) -> c_int;
    fn syscall(num: isize, ...) -> isize;
    fn sync();
    fn umount2(target: *const c_char, flags: c_int) -> c_int;
    fn usleep(usec: c_uint) -> c_int;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    fn _exit(status: c_int) -> !;
}

fn errno() -> c_int {
    Error::last_os_error().raw_os_error().unwrap_or(0)
}

macro_rules! log {
    ($($arg:tt)*) => {{
        log_line(format_args!($($arg)*));
    }};
}

fn die(msg: &str) -> ! {
    log!("{msg}: {}", Error::last_os_error());
    unsafe { _exit(125) }
}

fn log_line(args: fmt::Arguments<'_>) {
    let mut line = String::from("lnx-agent: ");
    let _ = fmt::write(&mut line, args);
    line.push('\n');
    write_all(STDERR_FILENO, line.as_bytes());
    let _ = fs::write("/dev/kmsg", line);
}

fn cstr(bytes: &[u8]) -> *const c_char {
    bytes.as_ptr() as *const c_char
}

fn mount_fs(source: &[u8], target: &[u8], fstype: &[u8], flags: c_ulong, data: &[u8]) {
    let data_ptr = if data.is_empty() {
        ptr::null()
    } else {
        cstr(data) as *const c_void
    };
    if unsafe { mount(cstr(source), cstr(target), cstr(fstype), flags, data_ptr) } < 0 {
        die("mount");
    }
}

fn mount_virtiofs_with_dax(tag: &str, guest_path: &str, read_only: bool, dax: bool) {
    if tag.is_empty() || guest_path.is_empty() || !guest_path.starts_with('/') {
        return;
    }
    let target = format!("/newroot{guest_path}");
    if fs::metadata(&target).is_err() {
        ensure_dir(&target);
    }
    let tag = CString::new(tag).unwrap();
    let target = CString::new(target).unwrap();
    let fstype = cstr(b"virtiofs\0");
    let flags = if read_only { MS_RDONLY } else { 0 };
    let data = if dax {
        cstr(b"dax\0") as *const c_void
    } else {
        ptr::null()
    };
    if unsafe { mount(tag.as_ptr(), target.as_ptr(), fstype, flags, data) } < 0 {
        die("mount virtiofs");
    }
}

fn mount_virtiofs(tag: &str, guest_path: &str, read_only: bool) {
    mount_virtiofs_with_dax(tag, guest_path, read_only, virtiofs_dax_enabled());
}

fn virtiofs_dax_enabled() -> bool {
    !matches!(
        env::var(VIRTIOFS_DAX_ENV).as_deref(),
        Ok("0" | "false" | "off" | "no")
    )
}

fn mount_host_shares() {
    if let Ok(home) = env::var("LNX_VIRTIOFS_HOME") {
        mount_virtiofs("home", &home, false);
    }
    if let Ok(cwd) = env::var("LNX_VIRTIOFS_CWD") {
        mount_virtiofs("cwd", &cwd, false);
    }
}

fn mount_vhost_user_fs() {
    let mounts = env::var("LNX_VHOST_USER_FS").unwrap_or_default();
    for mount in mounts.split(';').filter(|mount| !mount.is_empty()) {
        let parts = mount.split(':').collect::<Vec<_>>();
        let [tag, guest_path, mode] = parts.as_slice() else {
            log!("skipping malformed vhost-user fs mount: {mount}");
            continue;
        };
        if *mode != "ro" {
            log!("skipping non-read-only vhost-user fs mount: {mount}");
            continue;
        }
        mount_virtiofs_with_dax(tag, guest_path, true, false);
    }
}

fn mount_package_output() {
    if env::var("LNX_VIRTIOFS_NIX_ROOT").ok().as_deref() == Some("1") {
        let read_only = env::var("LNX_VIRTIOFS_NIX_ROOT_RW").ok().as_deref() != Some("1");
        mount_virtiofs("lnx-nix-root", "/run/lnx/nix", read_only);
    }
}

// The host shares only the store root (mounted at /run/lnx/nix); its store/
// and profiles/ subdirectories are bound to their canonical guest paths.
fn setup_package_profile() {
    clean_stale_package_links();
    if env::var("LNX_PACKAGE_PROFILE").ok().as_deref() != Some("1") {
        return;
    }
    bind_mount_read_only("/run/lnx/nix/store", "/nix/store");
    bind_mount_read_only("/run/lnx/nix/profiles", "/run/lnx/packages");
    link_package_binaries();
}

fn bind_mount_read_only(source: &str, target: &str) {
    let source_path = format!("/newroot{source}");
    let target_path = format!("/newroot{target}");
    if fs::metadata(&target_path).is_err() {
        ensure_dir(&target_path);
    }
    let source_c = CString::new(source_path).unwrap();
    let target_c = CString::new(target_path).unwrap();
    if unsafe {
        mount(
            source_c.as_ptr(),
            target_c.as_ptr(),
            ptr::null(),
            MS_BIND,
            ptr::null(),
        )
    } < 0
    {
        die("bind package mount");
    }
    if unsafe {
        mount(
            ptr::null(),
            target_c.as_ptr(),
            ptr::null(),
            MS_BIND | MS_REMOUNT | MS_RDONLY,
            ptr::null(),
        )
    } < 0
    {
        die("remount package mount read-only");
    }
}

// Package links live in /usr/local/bin of the persistent rootfs, so links
// from an earlier boot must be removed even when the store is now absent:
// otherwise they dangle. Only lnx-owned links (into /run/lnx/packages) are
// touched; regular files and foreign symlinks are left alone.
fn clean_stale_package_links() {
    let Ok(entries) = fs::read_dir("/newroot/usr/local/bin") else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(link) = fs::read_link(&path) else {
            continue;
        };
        if link.to_string_lossy().starts_with("/run/lnx/packages/") {
            let _ = fs::remove_file(&path);
        }
    }
}

fn link_package_binaries() {
    let profile = format!(
        "/newroot/run/lnx/packages/{}",
        lnx_protocol::PACKAGE_PROFILE_NAME
    );
    let Some(profile) = resolve_newroot_symlinks(profile) else {
        log!("package profile link loops");
        return;
    };
    let Some(bin_dir) = resolve_newroot_symlinks(format!("{profile}/bin")) else {
        log!("package profile bin link loops");
        return;
    };
    let Ok(entries) = fs::read_dir(&bin_dir) else {
        log!("package profile has no bin directory: {bin_dir}");
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let target = format!("/newroot/usr/local/bin/{name}");
        if fs::symlink_metadata(&target).is_ok() {
            continue;
        }
        let source = format!(
            "/run/lnx/packages/{}/bin/{name}",
            lnx_protocol::PACKAGE_PROFILE_NAME
        );
        let _ = std::os::unix::fs::symlink(&source, &target);
    }
}

// Profile symlinks point at guest-absolute /nix/store paths, but the agent
// runs before switch_root, so absolute link targets must be re-rooted under
// /newroot by hand.
fn resolve_newroot_symlinks(mut path: String) -> Option<String> {
    for _ in 0..16 {
        let link = match fs::read_link(&path) {
            Ok(link) => link,
            Err(_) => return Some(path),
        };
        let link = link.to_string_lossy();
        path = if link.starts_with('/') {
            format!("/newroot{link}")
        } else {
            let parent = &path[..path.rfind('/')?];
            format!("{parent}/{link}")
        };
    }
    None
}

fn restore_sync_guest_entropy(entropy: &[u8]) -> Result<(), String> {
    if entropy.len() < RESTORE_ENTROPY_MIN_BYTES {
        return Err(format!(
            "restore entropy too short: {} bytes, need at least {RESTORE_ENTROPY_MIN_BYTES}",
            entropy.len()
        ));
    }
    if entropy.len() > RESTORE_ENTROPY_MAX_BYTES {
        return Err(format!(
            "restore entropy too large: {} bytes, max {RESTORE_ENTROPY_MAX_BYTES}",
            entropy.len()
        ));
    }

    let random = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/random")
        .map_err(|e| format!("open /dev/random after restore: {e}"))?;
    let mut info = RandPoolInfo {
        entropy_count: (entropy.len() * 8)
            .try_into()
            .map_err(|_| "restore entropy bit count overflows int".to_string())?,
        buf_size: entropy
            .len()
            .try_into()
            .map_err(|_| "restore entropy size overflows int".to_string())?,
        buf: [0; RESTORE_ENTROPY_MAX_BYTES],
    };
    info.buf[..entropy.len()].copy_from_slice(entropy);

    if unsafe {
        ioctl(
            random.as_raw_fd(),
            RNDADDENTROPY,
            &info as *const RandPoolInfo,
        )
    } < 0
    {
        return Err(format!(
            "RNDADDENTROPY after restore: {}",
            Error::last_os_error()
        ));
    }

    if unsafe { ioctl(random.as_raw_fd(), RNDRESEEDCRNG, 0) } < 0 {
        let err = Error::last_os_error();
        match err.raw_os_error() {
            Some(EINVAL) | Some(ENOTTY) => {}
            _ => return Err(format!("RNDRESEEDCRNG after restore: {err}")),
        }
    }

    fs::write(VMSTATE_RESEED_MARKER, b"ok\n")
        .map_err(|e| format!("write {VMSTATE_RESEED_MARKER}: {e}"))?;
    Ok(())
}

fn restore_sync_guest_caches(entropy: &[u8]) -> Result<(), String> {
    restore_sync_guest_entropy(entropy)?;
    unsafe {
        sync();
    }
    fs::write("/proc/sys/vm/drop_caches", b"3\n")
        .map_err(|e| format!("drop guest caches after restore: {e}"))?;
    unsafe {
        sync();
    }
    Ok(())
}

fn wait_for_path(path: &str) -> bool {
    for _ in 0..100 {
        if fs::metadata(path).is_ok() {
            return true;
        }
        unsafe {
            usleep(50_000);
        }
    }
    false
}

fn ensure_dir(path: &str) {
    if let Err(e) = fs::create_dir_all(path) {
        eprintln!("create {path}: {e}");
        unsafe { _exit(125) }
    }
}

fn sync_clock_from_host() {
    let Ok(raw) = env::var("LNX_HOST_UNIX_SECS") else {
        return;
    };
    let Ok(tv_sec) = raw.parse::<i64>() else {
        return;
    };
    let ts = Timespec { tv_sec, tv_nsec: 0 };
    let _ = unsafe { clock_settime(CLOCK_REALTIME, &ts) };
}

/// The host can pass an explicit address override; without one the VM is
/// behind gvproxy NAT at the fixed gvproxy addresses.
fn network_config() -> (String, String) {
    match (env::var("LNX_NET_IP"), env::var("LNX_NET_GATEWAY")) {
        (Ok(ip), Ok(gateway)) if !ip.is_empty() && !gateway.is_empty() => (ip, gateway),
        _ => ("192.168.127.2/24".to_string(), "192.168.127.1".to_string()),
    }
}

fn configure_network() {
    let (ip, gateway) = network_config();
    match configure_network_direct(&ip, &gateway) {
        Ok(()) => log!("network.configured ip={ip} gateway={gateway}"),
        Err(e) => log!("network.configure.error {e}"),
    }
    let _ = fs::remove_file("/etc/resolv.conf");
    let _ = fs::write(
        "/etc/resolv.conf",
        format!("nameserver {gateway}\nnameserver 1.1.1.1\n"),
    );
    ensure_hosts();
}

struct OwnedFd {
    fd: c_int,
}

impl OwnedFd {
    fn new(fd: c_int) -> Self {
        Self { fd }
    }

    fn raw(&self) -> c_int {
        self.fd
    }
}

impl Drop for OwnedFd {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe {
                close(self.fd);
            }
        }
    }
}

fn configure_network_direct(ip: &str, gateway: &str) -> Result<(), String> {
    let (addr, prefix_len) = parse_ipv4_cidr(ip)?;
    let gateway = gateway
        .parse::<Ipv4Addr>()
        .map_err(|e| format!("parse gateway {gateway}: {e}"))?;
    let netlink = open_route_netlink()?;
    let lo = interface_index("lo")?;
    let eth0 = interface_index("eth0")?;
    let mut seq = 1;

    set_link_up(netlink.raw(), lo, seq)?;
    seq += 1;
    set_link_up(netlink.raw(), eth0, seq)?;
    seq += 1;
    replace_ipv4_address(netlink.raw(), eth0, addr, prefix_len, seq)?;
    seq += 1;
    replace_default_route(netlink.raw(), eth0, gateway, seq)
}

fn parse_ipv4_cidr(value: &str) -> Result<(Ipv4Addr, u8), String> {
    let (addr, prefix_len) = value
        .split_once('/')
        .ok_or_else(|| format!("IPv4 address must include a prefix length: {value}"))?;
    let addr = addr
        .parse::<Ipv4Addr>()
        .map_err(|e| format!("parse IPv4 address {addr}: {e}"))?;
    let prefix_len = prefix_len
        .parse::<u8>()
        .map_err(|e| format!("parse IPv4 prefix length {prefix_len}: {e}"))?;
    if prefix_len > 32 {
        return Err(format!("IPv4 prefix length out of range: {prefix_len}"));
    }
    Ok((addr, prefix_len))
}

fn open_route_netlink() -> Result<OwnedFd, String> {
    let fd = unsafe { socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE) };
    if fd < 0 {
        return Err(format!("socket(AF_NETLINK): {}", Error::last_os_error()));
    }
    let fd = OwnedFd::new(fd);
    let local = SockaddrNl {
        nl_family: AF_NETLINK as u16,
        nl_pad: 0,
        nl_pid: 0,
        nl_groups: 0,
    };
    if unsafe {
        bind(
            fd.raw(),
            &local as *const SockaddrNl as *const Sockaddr,
            size_of::<SockaddrNl>() as c_uint,
        )
    } < 0
    {
        return Err(format!("bind(AF_NETLINK): {}", Error::last_os_error()));
    }
    let kernel = SockaddrNl {
        nl_family: AF_NETLINK as u16,
        nl_pad: 0,
        nl_pid: 0,
        nl_groups: 0,
    };
    if unsafe {
        connect(
            fd.raw(),
            &kernel as *const SockaddrNl as *const Sockaddr,
            size_of::<SockaddrNl>() as c_uint,
        )
    } < 0
    {
        return Err(format!("connect(AF_NETLINK): {}", Error::last_os_error()));
    }
    Ok(fd)
}

fn interface_index(name: &str) -> Result<c_uint, String> {
    let name_c = CString::new(name).map_err(|_| format!("interface name contains NUL: {name}"))?;
    let index = unsafe { if_nametoindex(name_c.as_ptr()) };
    if index == 0 {
        return Err(format!(
            "interface {name} not found: {}",
            Error::last_os_error()
        ));
    }
    Ok(index)
}

fn set_link_up(fd: c_int, ifindex: c_uint, seq: u32) -> Result<(), String> {
    let payload = IfInfoMsg {
        ifi_family: AF_UNSPEC as u8,
        ifi_pad: 0,
        ifi_type: 0,
        ifi_index: ifindex as c_int,
        ifi_flags: IFF_UP,
        ifi_change: IFF_UP,
    };
    netlink_request(
        fd,
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK,
        seq,
        &payload,
        &[],
        false,
        "set link up",
    )
}

fn replace_ipv4_address(
    fd: c_int,
    ifindex: c_uint,
    addr: Ipv4Addr,
    prefix_len: u8,
    seq: u32,
) -> Result<(), String> {
    let payload = IfAddrMsg {
        ifa_family: AF_INET as u8,
        ifa_prefixlen: prefix_len,
        ifa_flags: 0,
        ifa_scope: RT_SCOPE_UNIVERSE,
        ifa_index: ifindex,
    };
    let addr = addr.octets();
    netlink_request(
        fd,
        RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_REPLACE,
        seq,
        &payload,
        &[(IFA_LOCAL, &addr), (IFA_ADDRESS, &addr)],
        true,
        "replace IPv4 address",
    )
}

fn replace_default_route(
    fd: c_int,
    ifindex: c_uint,
    gateway: Ipv4Addr,
    seq: u32,
) -> Result<(), String> {
    let payload = RtMsg {
        rtm_family: AF_INET as u8,
        rtm_dst_len: 0,
        rtm_src_len: 0,
        rtm_tos: 0,
        rtm_table: RT_TABLE_MAIN,
        rtm_protocol: RTPROT_BOOT,
        rtm_scope: RT_SCOPE_UNIVERSE,
        rtm_type: RTN_UNICAST,
        rtm_flags: 0,
    };
    let dst = [0u8; 4];
    let gateway = gateway.octets();
    let oif = (ifindex as c_int).to_ne_bytes();
    netlink_request(
        fd,
        RTM_NEWROUTE,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_REPLACE,
        seq,
        &payload,
        &[(RTA_OIF, &oif), (RTA_DST, &dst), (RTA_GATEWAY, &gateway)],
        true,
        "replace default route",
    )
}

fn netlink_request<T>(
    fd: c_int,
    message_type: u16,
    flags: u16,
    seq: u32,
    payload: &T,
    attrs: &[(u16, &[u8])],
    allow_exists: bool,
    label: &str,
) -> Result<(), String> {
    let mut buf = [0u8; 4096];
    let mut len = size_of::<NlMsgHdr>();
    put_struct(&mut buf, len, payload)?;
    len += size_of::<T>();
    for (attr_type, data) in attrs {
        add_netlink_attr(&mut buf, &mut len, *attr_type, data)?;
    }
    let header = NlMsgHdr {
        nlmsg_len: len as u32,
        nlmsg_type: message_type,
        nlmsg_flags: flags,
        nlmsg_seq: seq,
        nlmsg_pid: 0,
    };
    put_struct(&mut buf, 0, &header)?;
    netlink_write(fd, &buf[..len]).map_err(|e| format!("{label}: {e}"))?;
    netlink_ack(fd, seq, allow_exists).map_err(|e| format!("{label}: {e}"))
}

fn add_netlink_attr(
    buf: &mut [u8],
    len: &mut usize,
    attr_type: u16,
    data: &[u8],
) -> Result<(), String> {
    let start = align4(*len);
    let attr_len = size_of::<RtAttr>() + data.len();
    if attr_len > u16::MAX as usize {
        return Err("netlink attribute too large".to_string());
    }
    let end = start
        .checked_add(align4(attr_len))
        .ok_or_else(|| "netlink message length overflow".to_string())?;
    if end > buf.len() {
        return Err("netlink message too large".to_string());
    }
    let attr = RtAttr {
        rta_len: attr_len as u16,
        rta_type: attr_type,
    };
    put_struct(buf, start, &attr)?;
    put_bytes(buf, start + size_of::<RtAttr>(), data)?;
    *len = end;
    Ok(())
}

fn netlink_write(fd: c_int, buf: &[u8]) -> Result<(), String> {
    loop {
        let written = unsafe { write(fd, buf.as_ptr() as *const c_void, buf.len()) };
        if written < 0 {
            if errno() == EINTR {
                continue;
            }
            return Err(format!("write: {}", Error::last_os_error()));
        }
        if written as usize != buf.len() {
            return Err(format!("short write: {written} of {}", buf.len()));
        }
        return Ok(());
    }
}

fn netlink_read(fd: c_int, buf: &mut [u8]) -> Result<usize, String> {
    loop {
        let read_len = unsafe { read(fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
        if read_len < 0 {
            if errno() == EINTR {
                continue;
            }
            return Err(format!("read: {}", Error::last_os_error()));
        }
        if read_len == 0 {
            return Err("unexpected EOF".to_string());
        }
        return Ok(read_len as usize);
    }
}

fn netlink_ack(fd: c_int, seq: u32, allow_exists: bool) -> Result<(), String> {
    let mut buf = [0u8; 4096];
    loop {
        let len = netlink_read(fd, &mut buf)?;
        let mut offset = 0;
        while offset + size_of::<NlMsgHdr>() <= len {
            let header = read_struct::<NlMsgHdr>(&buf, offset)
                .ok_or_else(|| "short netlink header".to_string())?;
            if header.nlmsg_len < size_of::<NlMsgHdr>() as u32 {
                return Err("invalid netlink message length".to_string());
            }
            let message_end = offset
                .checked_add(header.nlmsg_len as usize)
                .ok_or_else(|| "netlink message length overflow".to_string())?;
            if message_end > len {
                return Err("truncated netlink message".to_string());
            }
            if header.nlmsg_seq == seq {
                match header.nlmsg_type {
                    NLMSG_ERROR => {
                        let error = read_struct::<c_int>(&buf, offset + size_of::<NlMsgHdr>())
                            .ok_or_else(|| "short netlink error".to_string())?;
                        if error == 0 {
                            return Ok(());
                        }
                        let code = if error < 0 { -error } else { error };
                        if allow_exists && code == EEXIST {
                            return Ok(());
                        }
                        return Err(format!(
                            "kernel error {code}: {}",
                            Error::from_raw_os_error(code)
                        ));
                    }
                    NLMSG_DONE => return Ok(()),
                    _ => {}
                }
            }
            offset += align4(header.nlmsg_len as usize);
        }
    }
}

fn put_struct<T>(buf: &mut [u8], offset: usize, value: &T) -> Result<(), String> {
    let len = size_of::<T>();
    let end = offset
        .checked_add(len)
        .ok_or_else(|| "buffer offset overflow".to_string())?;
    if end > buf.len() {
        return Err("buffer too small".to_string());
    }
    unsafe {
        ptr::copy_nonoverlapping(
            value as *const T as *const u8,
            buf[offset..end].as_mut_ptr(),
            len,
        );
    }
    Ok(())
}

fn put_bytes(buf: &mut [u8], offset: usize, data: &[u8]) -> Result<(), String> {
    let end = offset
        .checked_add(data.len())
        .ok_or_else(|| "buffer offset overflow".to_string())?;
    if end > buf.len() {
        return Err("buffer too small".to_string());
    }
    buf[offset..end].copy_from_slice(data);
    Ok(())
}

fn read_struct<T: Copy>(buf: &[u8], offset: usize) -> Option<T> {
    let end = offset.checked_add(size_of::<T>())?;
    if end > buf.len() {
        return None;
    }
    Some(unsafe { ptr::read_unaligned(buf[offset..end].as_ptr() as *const T) })
}

fn align4(value: usize) -> usize {
    (value + 3) & !3
}

/// Populate /etc/hosts so the local hostname resolves. Some base images ship
/// an empty hosts file with the hostname set to localhost.localdomain, which
/// makes getaddrinfo fail and tools like sudo warn "unable to resolve host".
/// Only writes when the file is empty so an image's own hosts is preserved.
fn ensure_hosts() {
    if !fs::read_to_string("/etc/hosts")
        .unwrap_or_default()
        .trim()
        .is_empty()
    {
        return;
    }
    let mut names = String::from("localhost localhost.localdomain");
    if let Some(hostname) = fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "localhost" && s != "localhost.localdomain")
    {
        names.push(' ');
        names.push_str(&hostname);
    }
    let _ = fs::write(
        "/etc/hosts",
        format!("127.0.0.1\t{names}\n::1\tlocalhost ip6-localhost ip6-loopback\n"),
    );
}

fn init_mode() -> ! {
    sync_clock_from_host();

    ensure_dir("/dev");
    ensure_dir("/proc");
    ensure_dir("/sys");
    ensure_dir("/newroot");

    let _ = unsafe {
        mount(
            cstr(b"devtmpfs\0"),
            cstr(b"/dev\0"),
            cstr(b"devtmpfs\0"),
            0,
            ptr::null(),
        )
    };
    let _ = unsafe {
        mount(
            cstr(b"proc\0"),
            cstr(b"/proc\0"),
            cstr(b"proc\0"),
            0,
            ptr::null(),
        )
    };
    let _ = unsafe {
        mount(
            cstr(b"sysfs\0"),
            cstr(b"/sys\0"),
            cstr(b"sysfs\0"),
            0,
            ptr::null(),
        )
    };
    let root_device = env::var("LNX_ROOT_DEVICE").unwrap_or_else(|_| "/dev/pmem0".to_string());
    if !wait_for_path(&root_device) {
        log!("timed out waiting for {root_device}");
    }
    let root_device =
        CString::new(root_device).unwrap_or_else(|_| CString::new("/dev/pmem0").unwrap());
    let root_options = if root_device.as_bytes() == b"/dev/pmem0" {
        b"errors=panic,dax\0".as_slice()
    } else {
        b"errors=panic\0".as_slice()
    };
    mount_fs(
        root_device.as_bytes_with_nul(),
        b"/newroot\0",
        b"ext4\0",
        0,
        root_options,
    );
    mount_host_shares();
    mount_vhost_user_fs();

    ensure_dir("/newroot/usr/local/lib/lnx");
    ensure_dir("/newroot/usr/local/bin");
    ensure_dir("/newroot/dev");
    ensure_dir("/newroot/proc");
    ensure_dir("/newroot/sys");
    ensure_dir("/newroot/run");
    for path in [
        OLD_AGENT_PATH,
        OLD_LNXCTL_PATH,
        OLD_SERVICE_PATH,
        OLD_WANTS_LINK,
    ] {
        let _ = fs::remove_file(format!("/newroot{path}"));
    }
    let _ = unsafe {
        mount(
            cstr(b"tmpfs\0"),
            cstr(b"/newroot/run\0"),
            cstr(b"tmpfs\0"),
            0,
            cstr(b"mode=0755\0") as *const c_void,
        )
    };
    ensure_dir("/newroot/run/lnx");
    mount_package_output();
    setup_package_profile();
    ensure_dir(&format!("/newroot{WANTS_DIR}"));

    for (source, target) in [
        ("/lnx-agent", format!("/newroot{AGENT_PATH}")),
        ("/lnxctl", format!("/newroot{LNXCTL_PATH}")),
    ] {
        if let Err(e) = fs::write(&target, []) {
            log!("create bind target {target}: {e}");
            unsafe { _exit(125) }
        }
        let source = CString::new(source).unwrap();
        let target_c = CString::new(target.clone()).unwrap();
        if unsafe {
            mount(
                source.as_ptr(),
                target_c.as_ptr(),
                ptr::null(),
                MS_BIND,
                ptr::null(),
            )
        } < 0
        {
            die("bind mount lnx payload");
        }
        if let Err(e) = fs::set_permissions(&target, fs::Permissions::from_mode(0o755)) {
            log!("chmod bind target {target}: {e}");
            unsafe { _exit(125) }
        }
    }
    let lnxctl_link = CString::new(format!("/newroot{OLD_LNXCTL_PATH}")).unwrap();
    let _ = unsafe { symlink(cstr(b"/run/lnx/lnxctl\0"), lnxctl_link.as_ptr()) };
    let xdg_open_link = CString::new(format!("/newroot{XDG_OPEN_PATH}")).unwrap();
    let _ = fs::remove_file(format!("/newroot{XDG_OPEN_PATH}"));
    let _ = unsafe { symlink(cstr(b"/run/lnx/lnxctl\0"), xdg_open_link.as_ptr()) };

    let unit = concat!(
        "[Unit]\n",
        "Description=lnx guest agent\n",
        "After=basic.target\n",
        "Before=multi-user.target\n",
        "\n",
        "[Service]\n",
        "Type=simple\n",
        "ExecStart=/run/lnx/lnx-agent --agent 10240\n",
        "Environment=LNX_CONTROL_SOCKET=/run/lnx-agent.sock\n",
        "StandardOutput=journal+console\n",
        "StandardError=journal+console\n",
        "Restart=always\n",
        "RestartSec=1\n",
        "\n",
        "[Install]\n",
        "WantedBy=multi-user.target\n",
    );
    if let Err(e) = fs::write(format!("/newroot{SERVICE_PATH}"), unit) {
        log!("write lnx-agent.service: {e}");
        unsafe { _exit(125) }
    }
    let _ = fs::remove_file(format!("/newroot{WANTS_LINK}"));
    if unsafe { symlink(cstr(b"../lnx-agent.service\0"), cstr(WANTS_LINK_C)) } < 0 {
        die("symlink lnx-agent.service");
    }

    if unsafe {
        mount(
            cstr(b"/dev\0"),
            cstr(b"/newroot/dev\0"),
            cstr(b"devtmpfs\0"),
            0,
            ptr::null(),
        )
    } < 0
    {
        die("mount /newroot/dev");
    }
    if unsafe {
        mount(
            cstr(b"/proc\0"),
            cstr(b"/newroot/proc\0"),
            cstr(b"proc\0"),
            0,
            ptr::null(),
        )
    } < 0
    {
        die("mount /newroot/proc");
    }
    if unsafe {
        mount(
            cstr(b"/sys\0"),
            cstr(b"/newroot/sys\0"),
            cstr(b"sysfs\0"),
            0,
            ptr::null(),
        )
    } < 0
    {
        die("mount /newroot/sys");
    }
    ensure_dir("/newroot/dev/pts");
    if unsafe {
        mount(
            cstr(b"devpts\0"),
            cstr(b"/newroot/dev/pts\0"),
            cstr(b"devpts\0"),
            0,
            cstr(b"newinstance,ptmxmode=0666,mode=0620\0") as *const c_void,
        )
    } < 0
    {
        die("mount /newroot/dev/pts");
    }
    let _ = fs::remove_file("/newroot/dev/ptmx");
    if unsafe { symlink(cstr(b"pts/ptmx\0"), cstr(b"/newroot/dev/ptmx\0")) } < 0
        && errno() != EEXIST
    {
        die("symlink /newroot/dev/ptmx");
    }
    ensure_dir("/newroot/dev/shm");
    let _ = unsafe {
        mount(
            cstr(b"tmpfs\0"),
            cstr(b"/newroot/dev/shm\0"),
            cstr(b"tmpfs\0"),
            0,
            ptr::null(),
        )
    };
    ensure_dir("/newroot/tmp");
    let _ = unsafe {
        mount(
            cstr(b"tmpfs\0"),
            cstr(b"/newroot/tmp\0"),
            cstr(b"tmpfs\0"),
            0,
            ptr::null(),
        )
    };

    if unsafe {
        mount(
            ptr::null(),
            cstr(b"/\0"),
            ptr::null(),
            MS_REC | MS_PRIVATE,
            ptr::null(),
        )
    } < 0
    {
        die("make mounts private");
    }

    ensure_dir("/newroot/oldroot");
    if unsafe {
        syscall(
            SYS_PIVOT_ROOT,
            cstr(b"/newroot\0"),
            cstr(b"/newroot/oldroot\0"),
        )
    } < 0
    {
        die("pivot_root");
    }
    if unsafe { chdir(cstr(b"/\0")) } < 0 {
        die("chdir");
    }
    let _ = unsafe { umount2(cstr(b"/oldroot\0"), MNT_DETACH) };
    let _ = fs::remove_dir("/oldroot");

    configure_network();

    match detect_image_init() {
        // systemd reads the unit installed above and supervises the agent.
        Some(init) if init_is_systemd(&init) => exec_init(&init),
        Some(init) => {
            log!("image init {init} is not systemd; supervising agent directly");
            spawn_agent_supervisor();
            exec_init(&init)
        }
        None => {
            log!("image ships no init; lnx-agent stays pid 1");
            run_pid1_supervisor()
        }
    }
}

fn detect_image_init() -> Option<String> {
    ["/sbin/init", "/usr/sbin/init", "/etc/init", "/bin/init"]
        .into_iter()
        .find(|path| fs::metadata(path).is_ok())
        .map(str::to_string)
}

fn init_is_systemd(init: &str) -> bool {
    let mut path = std::path::PathBuf::from(init);
    for _ in 0..8 {
        match fs::read_link(&path) {
            Ok(target) if target.is_absolute() => path = target,
            Ok(target) => {
                path = path
                    .parent()
                    .unwrap_or(std::path::Path::new("/"))
                    .join(target)
            }
            Err(_) => break,
        }
    }
    path.file_name().is_some_and(|name| name == "systemd")
}

fn exec_init(init: &str) -> ! {
    let init = CString::new(init).unwrap();
    let argv = [init.as_ptr(), ptr::null()];
    unsafe {
        execvp(init.as_ptr(), argv.as_ptr());
    }
    die("exec image init")
}

fn exec_agent_service() -> ! {
    unsafe {
        setenv(
            cstr(b"LNX_CONTROL_SOCKET\0"),
            cstr(b"/run/lnx-agent.sock\0"),
            1,
        );
    }
    let agent = cstr(b"/run/lnx/lnx-agent\0");
    let argv = [agent, cstr(b"--agent\0"), cstr(b"10240\0"), ptr::null()];
    unsafe {
        execvp(agent, argv.as_ptr());
        _exit(125)
    }
}

fn spawn_agent_child() -> c_int {
    let pid = unsafe { fork() };
    if pid == 0 {
        exec_agent_service();
    }
    pid
}

/// For images whose init will not start the agent for us: keep an agent
/// running from a supervisor forked off pid 1. The supervisor is inherited by
/// the image init when we exec it.
fn spawn_agent_supervisor() {
    let pid = unsafe { fork() };
    if pid < 0 {
        die("fork agent supervisor");
    }
    if pid > 0 {
        return;
    }
    unsafe {
        setsid();
    }
    loop {
        let agent = spawn_agent_child();
        if agent > 0 {
            let mut status = 0;
            unsafe {
                waitpid(agent, &mut status, 0);
            }
            log!("agent exited status={}; restarting", exit_status(status));
        }
        unsafe {
            usleep(1_000_000);
        }
    }
}

/// For images with no init at all (plain container images): stay pid 1,
/// reap every orphan, and keep the agent alive as our child. Exec channels
/// are children of the agent process, so reaping here cannot steal their
/// exit statuses.
fn run_pid1_supervisor() -> ! {
    let mut agent = spawn_agent_child();
    loop {
        let mut status = 0;
        let pid = unsafe { waitpid(-1, &mut status, 0) };
        if pid == agent || (pid < 0 && errno() == ECHILD) {
            unsafe {
                usleep(1_000_000);
            }
            agent = spawn_agent_child();
        }
    }
}

fn try_connect_vsock(port: u32, attempts: usize) -> c_int {
    log!("vsock.connect.begin port={port}");
    for attempt in 0..attempts {
        let addr = vsock_addr(port);
        let fd = unsafe { socket(AF_VSOCK, SOCK_STREAM, 0) };
        if fd < 0 {
            die("socket(AF_VSOCK)");
        }
        set_cloexec(fd);
        let ret = unsafe {
            connect(
                fd,
                &addr as *const SockaddrVm as *const Sockaddr,
                size_of::<SockaddrVm>() as c_uint,
            )
        };
        if ret == 0 {
            log!("vsock.connect.success port={port} attempt={attempt} fd={fd}");
            return fd;
        }
        if attempt == 0 || attempt + 1 == attempts || attempt % 50 == 49 {
            log!(
                "vsock.connect.retry port={port} attempt={attempt} errno={}",
                errno()
            );
        }
        unsafe {
            close(fd);
        }
        unsafe {
            usleep(100_000);
        }
    }
    -1
}

fn connect_vsock(port: u32) -> c_int {
    let fd = try_connect_vsock(port, 600);
    if fd >= 0 {
        return fd;
    }
    die("connect(vsock)")
}

fn write_all(fd: c_int, mut buf: &[u8]) -> bool {
    while !buf.is_empty() {
        let n = unsafe { write(fd, buf.as_ptr() as *const c_void, buf.len()) };
        if n < 0 {
            if errno() == EINTR {
                continue;
            }
            if errno() == EAGAIN {
                unsafe {
                    usleep(1_000);
                }
                continue;
            }
            return false;
        }
        buf = &buf[n as usize..];
    }
    true
}

fn write_frame(fd: c_int, frame_type: u8, payload: &[u8]) -> bool {
    write_all(fd, &[frame_type])
        && write_all(fd, &(payload.len() as u32).to_be_bytes())
        && write_all(fd, payload)
}

fn request_snapshot(fd: c_int) {
    write_frame(fd, FRAME_SNAPSHOT, &[]);
}

fn lnxctl_mode(args: &[String]) -> ! {
    if args.get(1).map(String::as_str) != Some("snapshot-exit") {
        write_all(STDERR_FILENO, b"usage: lnxctl snapshot-exit\n");
        unsafe { _exit(2) }
    }
    let socket = env::var(CONTROL_SOCKET_ENV).unwrap_or_else(|_| CONTROL_SOCKET.to_string());
    let fd = connect_unix(&socket);
    if !write_frame(fd, FRAME_CONTROL_SNAPSHOT_EXIT, &[]) {
        unsafe { _exit(1) }
    }
    let status = read_local_control_response(fd);
    unsafe { _exit(status) }
}

fn xdg_open_mode(args: &[String]) -> ! {
    let Some(target) = args.get(1) else {
        write_all(STDERR_FILENO, b"usage: xdg-open URL\n");
        unsafe { _exit(2) }
    };
    if !target.contains("://") {
        write_all(STDERR_FILENO, b"xdg-open: only URL targets are supported\n");
        unsafe { _exit(2) }
    }
    if target.len() > MAX_CONTROL_PAYLOAD {
        write_all(STDERR_FILENO, b"xdg-open: URL is too long\n");
        unsafe { _exit(2) }
    }
    let socket = env::var(CONTROL_SOCKET_ENV).unwrap_or_else(|_| CONTROL_SOCKET.to_string());
    let fd = connect_unix(&socket);
    if !write_frame(fd, FRAME_CONTROL_OPEN_URL, target.as_bytes()) {
        unsafe { _exit(1) }
    }
    let status = read_local_control_response(fd);
    unsafe { _exit(status) }
}

fn sockaddr_un(path: &str) -> SockaddrUn {
    let mut addr = SockaddrUn {
        sun_family: AF_UNIX as u16,
        sun_path: [0; 108],
    };
    let bytes = path.as_bytes();
    let len = bytes.len().min(addr.sun_path.len() - 1);
    addr.sun_path[..len].copy_from_slice(&bytes[..len]);
    addr
}

fn connect_unix(path: &str) -> c_int {
    let fd = unsafe { socket(AF_UNIX, SOCK_STREAM, 0) };
    if fd < 0 {
        die("socket(AF_UNIX)");
    }
    set_cloexec(fd);
    let addr = sockaddr_un(path);
    for _ in 0..100 {
        let ret = unsafe {
            connect(
                fd,
                &addr as *const SockaddrUn as *const Sockaddr,
                size_of::<SockaddrUn>() as c_uint,
            )
        };
        if ret == 0 {
            return fd;
        }
        unsafe {
            usleep(50_000);
        }
    }
    die("connect(AF_UNIX)")
}

fn listen_unix(path: &str) -> c_int {
    let _ = fs::remove_file(path);
    let fd = unsafe { socket(AF_UNIX, SOCK_STREAM, 0) };
    if fd < 0 {
        die("socket(AF_UNIX)");
    }
    set_cloexec(fd);
    let addr = sockaddr_un(path);
    if unsafe {
        bind(
            fd,
            &addr as *const SockaddrUn as *const Sockaddr,
            size_of::<SockaddrUn>() as c_uint,
        )
    } < 0
    {
        die("bind(AF_UNIX)");
    }
    if unsafe { listen(fd, 8) } < 0 {
        die("listen(AF_UNIX)");
    }
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o666));
    set_nonblocking(fd);
    fd
}

fn read_local_control_response(fd: c_int) -> c_int {
    let mut frame_type = [0u8; 1];
    read_exact(fd, &mut frame_type);
    let len = read_u32(fd);
    if frame_type[0] == FRAME_CONTROL_OK && len == 0 {
        0
    } else {
        1
    }
}

fn set_snapshot_resume_read_timeout(fd: c_int) {
    let timeout = Timeval {
        tv_sec: SNAPSHOT_RESUME_READ_TIMEOUT_USECS / 1_000_000,
        tv_usec: SNAPSHOT_RESUME_READ_TIMEOUT_USECS % 1_000_000,
    };
    if unsafe {
        setsockopt(
            fd,
            SOL_SOCKET,
            SO_RCVTIMEO,
            &timeout as *const Timeval as *const c_void,
            size_of::<Timeval>() as c_uint,
        )
    } < 0
    {
        die("setsockopt(SO_RCVTIMEO)");
    }
}

fn snapshot_resume_wait(fd: c_int) {
    log!("snapshot_resume_wait.begin fd={fd}");
    set_snapshot_resume_read_timeout(fd);
    let ready_written = write_all(fd, b"R");
    log!("snapshot_resume_wait.ready_written fd={fd} ok={ready_written}");
    let mut buf = [0u8; 1];
    loop {
        let n = unsafe { read(fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
        if n < 0 && errno() == EINTR {
            continue;
        }
        log!(
            "snapshot_resume_wait.read_done fd={fd} n={n} errno={}",
            errno()
        );
        break;
    }
}

fn request_snapshot_and_reconnect() -> c_int {
    log!("snapshot_reconnect.begin");
    let fd = connect_vsock(SNAPSHOT_PORT);
    log!("snapshot_reconnect.snapshot_connected fd={fd}");
    request_snapshot(fd);
    let resume_fd = try_connect_vsock(SNAPSHOT_PORT, 20);
    if resume_fd >= 0 {
        log!("snapshot_reconnect.resume_connected fd={resume_fd}");
        snapshot_resume_wait(resume_fd);
        log!("snapshot_reconnect.snapshot_wait_done fd={resume_fd}");
        unsafe {
            close(resume_fd);
        }
    } else {
        log!("snapshot_reconnect.resume_fallback fd={fd}");
        snapshot_resume_wait(fd);
        log!("snapshot_reconnect.snapshot_wait_done fd={fd}");
    }
    unsafe {
        close(fd);
    }
    let fd = reconnect_after_snapshot_point();
    log!("snapshot_reconnect.agent_connected fd={fd}");
    fd
}

fn reconnect_after_snapshot_point() -> c_int {
    connect_vsock(AGENT_PORT)
}

fn read_exact(fd: c_int, mut buf: &mut [u8]) {
    while !buf.is_empty() {
        let n = unsafe { read(fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
        if n < 0 {
            if errno() == EINTR {
                continue;
            }
            die("read");
        }
        if n == 0 {
            die("short read");
        }
        let tmp = buf;
        buf = &mut tmp[n as usize..];
    }
}

fn try_read_exact(fd: c_int, mut buf: &mut [u8]) -> bool {
    while !buf.is_empty() {
        let n = unsafe { read(fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
        if n < 0 {
            let err = errno();
            if err == EINTR {
                continue;
            }
            if err == EAGAIN {
                unsafe {
                    usleep(1_000);
                }
                continue;
            }
            log!("read_exact.error fd={fd} errno={err}");
            return false;
        }
        if n == 0 {
            log!("read_exact.eof fd={fd}");
            return false;
        }
        let tmp = buf;
        buf = &mut tmp[n as usize..];
    }
    true
}

fn read_u32(fd: c_int) -> u32 {
    let mut buf = [0u8; 4];
    read_exact(fd, &mut buf);
    u32::from_be_bytes(buf)
}

fn read_message(fd: c_int) -> Option<Message> {
    let mut len = [0u8; 4];
    if !try_read_exact(fd, &mut len) {
        return None;
    }
    let len = u32::from_be_bytes(len);
    if len > MAX_MESSAGE_SIZE {
        die("message too large");
    }
    let mut buf = vec![0u8; len as usize];
    if len > 0 && !try_read_exact(fd, &mut buf) {
        return None;
    }
    postcard::from_bytes(&buf).ok()
}

fn write_message(fd: c_int, message: &Message) -> bool {
    let Ok(buf) = postcard::to_allocvec(message) else {
        return false;
    };
    if buf.len() > MAX_MESSAGE_SIZE as usize {
        return false;
    }
    let len = (buf.len() as u32).to_be_bytes();
    write_all(fd, &len) && write_all(fd, &buf)
}

fn write_message_locked(agent_fd: &Arc<Mutex<c_int>>, message: &Message) -> bool {
    let Ok(fd) = agent_fd.lock() else {
        return false;
    };
    write_message(*fd, message)
}

fn publish_agent_fd_and_hello(agent_fd: &Arc<Mutex<c_int>>, fd: c_int) -> bool {
    let Ok(mut shared) = agent_fd.lock() else {
        return false;
    };
    *shared = fd;
    write_message(
        *shared,
        &Message::Hello {
            version: PROTOCOL_VERSION,
        },
    )
}

fn listening_tcp_ports() -> Vec<u16> {
    let mut ports = Vec::new();
    collect_listening_tcp_ports("/proc/net/tcp", false, &mut ports);
    collect_listening_tcp_ports("/proc/net/tcp6", true, &mut ports);
    ports.sort_unstable();
    ports.dedup();
    ports
}

fn collect_listening_tcp_ports(path: &str, ipv6: bool, ports: &mut Vec<u16>) {
    let Ok(raw) = fs::read_to_string(path) else {
        return;
    };
    for line in raw.lines().skip(1) {
        if let Some(port) = parse_proc_net_tcp_listener(line, ipv6) {
            ports.push(port);
        }
    }
}

fn parse_proc_net_tcp_listener(line: &str, ipv6: bool) -> Option<u16> {
    let mut fields = line.split_whitespace();
    let _sl = fields.next()?;
    let local = fields.next()?;
    let _remote = fields.next()?;
    let state = fields.next()?;
    if state != "0A" {
        return None;
    }
    let (addr, port_hex) = local.split_once(':')?;
    let port = u16::from_str_radix(port_hex, 16).ok()?;
    if port <= 1024 {
        return None;
    }
    let local = if ipv6 {
        is_local_tcp6_addr(addr)
    } else {
        is_local_tcp4_addr(addr)
    };
    local.then_some(port)
}

fn is_local_tcp4_addr(addr: &str) -> bool {
    matches!(addr, "00000000" | "0100007F")
}

fn is_local_tcp6_addr(addr: &str) -> bool {
    if addr.len() != 32 {
        return false;
    }
    addr == "00000000000000000000000000000000" || addr == "00000000000000000000000001000000"
}

fn start_listener_monitor(agent_fd: Arc<Mutex<c_int>>) {
    thread::spawn(move || {
        let mut last = Vec::new();
        loop {
            let ports = listening_tcp_ports();
            if ports != last {
                let _ = write_message_locked(
                    &agent_fd,
                    &Message::PortListeners {
                        ports: ports.clone(),
                    },
                );
                last = ports;
            }
            thread::sleep(Duration::from_millis(LISTENER_MONITOR_INTERVAL_MS));
        }
    });
}

fn set_nonblocking(fd: c_int) -> bool {
    let flags = unsafe { fcntl(fd, F_GETFL) };
    if flags >= 0 {
        return unsafe { fcntl(fd, F_SETFL, flags | O_NONBLOCK) } == 0;
    }
    false
}

fn set_cloexec(fd: c_int) {
    let flags = unsafe { fcntl(fd, F_GETFD) };
    if flags >= 0 {
        unsafe {
            fcntl(fd, F_SETFD, flags | FD_CLOEXEC);
        }
    }
}

fn make_pipe(fds: &mut [c_int; 2]) -> bool {
    if unsafe { pipe(fds.as_mut_ptr()) } < 0 {
        return false;
    }
    set_cloexec(fds[0]);
    set_cloexec(fds[1]);
    true
}

fn close_if_open(fd: &mut c_int) {
    if *fd >= 0 {
        unsafe {
            close(*fd);
        }
        *fd = -1;
    }
}

fn close_pipe(fds: &mut [c_int; 2]) {
    close_if_open(&mut fds[0]);
    close_if_open(&mut fds[1]);
}

fn vsock_addr(port: u32) -> SockaddrVm {
    SockaddrVm {
        svm_family: AF_VSOCK as u16,
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: VMADDR_CID_HOST,
        svm_flags: 0,
        svm_zero: [0; 3],
    }
}

fn accept_channel_control(listener_fd: c_int) -> Option<ChannelControlRequest> {
    let client_fd = unsafe { accept(listener_fd, ptr::null_mut(), ptr::null_mut()) };
    if client_fd < 0 {
        return None;
    }
    set_cloexec(client_fd);
    let mut frame_type = [0u8; 1];
    let mut len = [0u8; 4];
    if !try_read_exact(client_fd, &mut frame_type) || !try_read_exact(client_fd, &mut len) {
        unsafe {
            close(client_fd);
        }
        return None;
    }
    let len = u32::from_be_bytes(len) as usize;
    match (frame_type[0], len) {
        (FRAME_CONTROL_SNAPSHOT_EXIT, 0) => {
            unsafe {
                sync();
            }
            Some(ChannelControlRequest::SnapshotExit(client_fd))
        }
        (FRAME_CONTROL_OPEN_URL, 1..=MAX_CONTROL_PAYLOAD) => {
            let mut payload = vec![0u8; len];
            if try_read_exact(client_fd, &mut payload) {
                if let Ok(url) = String::from_utf8(payload) {
                    return Some(ChannelControlRequest::OpenUrl { fd: client_fd, url });
                }
            }
            unsafe {
                close(client_fd);
            }
            None
        }
        _ => {
            unsafe {
                close(client_fd);
            }
            None
        }
    }
}

fn complete_pending_control(pending_control_fd: &mut c_int, ok: bool) {
    if *pending_control_fd < 0 {
        return;
    }
    if ok {
        let _ = write_frame(*pending_control_fd, FRAME_CONTROL_OK, &[]);
    }
    unsafe {
        close(*pending_control_fd);
    }
    *pending_control_fd = -1;
}

fn handle_channel_control(
    request: ChannelControlRequest,
    pending_control_fd: &mut c_int,
    agent_fd: &Arc<Mutex<c_int>>,
    channel_id: u64,
) {
    match request {
        ChannelControlRequest::SnapshotExit(fd) => {
            *pending_control_fd = fd;
            let _ = write_message_locked(agent_fd, &Message::SnapshotExit { channel_id });
        }
        ChannelControlRequest::OpenUrl { fd, url } => {
            *pending_control_fd = fd;
            let _ = write_message_locked(agent_fd, &Message::OpenUrl { channel_id, url });
        }
    }
}

fn close_control_fd(fd: c_int) {
    if fd >= 0 {
        unsafe {
            close(fd);
        }
    }
}

fn channel_control_socket(channel_id: u64) -> String {
    format!("/run/lnx-agent-{channel_id:016x}.sock")
}

fn close_channel_control_socket(fd: c_int, path: &str) {
    unsafe {
        close(fd);
    }
    let _ = fs::remove_file(path);
}

fn set_lnx_control_socket(path: &str) {
    if let (Ok(name), Ok(value)) = (CString::new(CONTROL_SOCKET_ENV), CString::new(path)) {
        unsafe {
            setenv(name.as_ptr(), value.as_ptr(), 1);
        }
    }
}

fn set_lnx_request_id(request_id: u64) {
    if let Ok(name) = CString::new("LNX_REQUEST_ID") {
        if let Ok(value) = CString::new(format!("{request_id:016x}")) {
            unsafe {
                setenv(name.as_ptr(), value.as_ptr(), 1);
            }
        }
    }
}

fn set_default_exec_environment() {
    if let (Ok(name), Ok(value)) = (CString::new("PATH"), CString::new(DEFAULT_PATH)) {
        unsafe {
            setenv(name.as_ptr(), value.as_ptr(), 1);
        }
    }
    set_env("HOME", EXEC_HOME);
    set_env("USER", EXEC_USER);
    set_env("LOGNAME", EXEC_USER);
    set_env("BROWSER", DEFAULT_BROWSER);
}

fn set_forwarded_environment(env: &[(String, String)]) {
    for (name, value) in env {
        if allowed_forwarded_env(name) {
            set_env(name, value);
        }
    }
}

fn allowed_forwarded_env(name: &str) -> bool {
    matches!(
        name,
        "TERM"
            | "COLORTERM"
            | "LANG"
            | "LANGUAGE"
            | "TZ"
            | "NO_COLOR"
            | "CLICOLOR"
            | "CLICOLOR_FORCE"
            | "LNX_INSTANCE"
            | "LNX_INGRESS_DOMAIN"
    ) || name.starts_with("LC_")
}

fn set_env(name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (CString::new(name), CString::new(value)) {
        unsafe {
            setenv(name.as_ptr(), value.as_ptr(), 1);
        }
    }
}

fn drop_to_exec_user(uid: u32, gid: u32) {
    if uid == 0 {
        return;
    }
    unsafe {
        if setgid(gid) < 0 {
            die("setgid");
        }
        if setuid(uid) < 0 {
            die("setuid");
        }
    }
}

/// Relax /dev/kvm so an unprivileged exec can run a nested VM. The image's
/// udev rule sets it to 0660 root:kvm, and the exec user isn't in the kvm
/// group; /dev/kvm also only appears once nested virtualization is engaged,
/// so relax it right before each exec.
fn relax_nested_kvm() {
    let _ = fs::set_permissions("/dev/kvm", fs::Permissions::from_mode(0o666));
}

fn exec_failed(arg0: *const c_char) -> ! {
    unsafe {
        let err = errno();
        let msg = CStr::from_ptr(arg0).to_bytes();
        if err == ENOENT {
            write_all(STDERR_FILENO, b"command not found: ");
        } else {
            write_all(STDERR_FILENO, b"exec failed: ");
        }
        write_all(STDERR_FILENO, msg);
        write_all(STDERR_FILENO, b"\n");
        _exit(127);
    }
}

fn child_die(message: &[u8]) -> ! {
    write_all(STDERR_FILENO, message);
    unsafe { _exit(125) }
}

/// Post-fork-safe decimal write (no allocation).
fn write_decimal(fd: c_int, mut value: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    write_all(fd, &buf[i..]);
}

fn drop_to_exec_user_child(uid: u32, gid: u32) {
    if uid == 0 {
        return;
    }
    unsafe {
        if setgid(gid) < 0 {
            child_die(b"setgid failed\n");
        }
        if setuid(uid) < 0 {
            child_die(b"setuid failed\n");
        }
    }
}

fn exit_status(status: c_int) -> c_int {
    if (status & 0x7f) == 0 {
        (status >> 8) & 0xff
    } else {
        128 + (status & 0x7f)
    }
}

fn make_argv(argv: &[String]) -> (Vec<CString>, Vec<*const c_char>) {
    let storage = argv
        .iter()
        .map(|arg| CString::new(arg.as_str()).unwrap_or_else(|_| CString::new("").unwrap()))
        .collect::<Vec<_>>();
    let mut ptrs = storage.iter().map(|arg| arg.as_ptr()).collect::<Vec<_>>();
    ptrs.push(ptr::null());
    (storage, ptrs)
}

// The *_storage fields look unused but own the CStrings the *_ptrs arrays
// point into; everything is materialized before fork so the child only
// touches pre-built memory.
#[allow(dead_code)]
struct ChildExec {
    argv_storage: Vec<CString>,
    argv_ptrs: Vec<*const c_char>,
    env_storage: Vec<CString>,
    env_ptrs: Vec<*const c_char>,
    cwd: Option<CString>,
    exec_paths: Vec<CString>,
}

fn push_env(storage: &mut Vec<CString>, name: &str, value: &str) {
    if let Ok(entry) = CString::new(format!("{name}={value}")) {
        storage.push(entry);
    }
}

/// An empty argv asks for a login shell; resolve it from the exec user's
/// passwd entry so images without bash still get their own shell.
fn resolve_login_shell(argv: Vec<String>, uid: u32) -> Vec<String> {
    if !argv.is_empty() {
        return argv;
    }
    vec![user::login_shell_for_uid(uid), "-l".to_string()]
}

fn make_exec_paths(argv0: &str) -> Vec<CString> {
    if argv0.contains('/') {
        return CString::new(argv0)
            .map(|path| vec![path])
            .unwrap_or_else(|_| vec![CString::new("").unwrap()]);
    }
    DEFAULT_PATH
        .split(':')
        .filter_map(|dir| CString::new(format!("{dir}/{argv0}")).ok())
        .collect()
}

fn make_child_exec(
    argv: &[String],
    cwd: &str,
    env: &[(String, String)],
    channel_id: u64,
    control_socket: &str,
) -> ChildExec {
    let (argv_storage, argv_ptrs) = make_argv(argv);
    let argv0 = argv.first().map(String::as_str).unwrap_or("");
    let mut env_storage = Vec::new();
    push_env(&mut env_storage, "PATH", DEFAULT_PATH);
    push_env(&mut env_storage, "HOME", EXEC_HOME);
    push_env(&mut env_storage, "USER", EXEC_USER);
    push_env(&mut env_storage, "LOGNAME", EXEC_USER);
    push_env(&mut env_storage, "BROWSER", DEFAULT_BROWSER);
    push_env(
        &mut env_storage,
        "LNX_REQUEST_ID",
        &format!("{channel_id:016x}"),
    );
    push_env(&mut env_storage, CONTROL_SOCKET_ENV, control_socket);
    for (name, value) in env {
        if allowed_forwarded_env(name) {
            push_env(&mut env_storage, name, value);
        }
    }
    let mut env_ptrs = env_storage
        .iter()
        .map(|entry| entry.as_ptr())
        .collect::<Vec<_>>();
    env_ptrs.push(ptr::null());
    let cwd = if cwd.is_empty() {
        None
    } else {
        CString::new(cwd).ok()
    };
    ChildExec {
        argv_storage,
        argv_ptrs,
        env_storage,
        env_ptrs,
        cwd,
        exec_paths: make_exec_paths(argv0),
    }
}

fn log_child_probe(channel_id: u64, pid: c_int) {
    let base = format!("/proc/{pid}");
    let comm = fs::read_to_string(format!("{base}/comm")).unwrap_or_else(|_| "?".to_string());
    let wchan = fs::read_to_string(format!("{base}/wchan")).unwrap_or_else(|_| "?".to_string());
    let status = fs::read_to_string(format!("{base}/status")).unwrap_or_default();
    let state = status
        .lines()
        .find(|line| line.starts_with("State:"))
        .unwrap_or("State: ?");
    let voluntary = status
        .lines()
        .find(|line| line.starts_with("voluntary_ctxt_switches:"))
        .unwrap_or("voluntary_ctxt_switches: ?");
    let nonvoluntary = status
        .lines()
        .find(|line| line.starts_with("nonvoluntary_ctxt_switches:"))
        .unwrap_or("nonvoluntary_ctxt_switches: ?");
    log!(
        "channel.pipe.child_probe channel={channel_id:016x} pid={pid} comm={} wchan={} {} {} {}",
        comm.trim(),
        wchan.trim(),
        state,
        voluntary,
        nonvoluntary
    );
}

fn send_status(agent_fd: &Arc<Mutex<c_int>>, channel_id: u64, status: c_int) {
    log!(
        "channel.status.send channel={channel_id:016x} status={}",
        exit_status(status)
    );
    let _ = write_message_locked(
        agent_fd,
        &Message::ExitStatus {
            channel_id,
            status: exit_status(status),
        },
    );
    let _ = write_message_locked(agent_fd, &Message::Close { channel_id });
}

fn poll_output(output_fd: c_int) -> Option<i16> {
    let mut fd = PollFd {
        fd: output_fd,
        events: POLLIN | POLLERR | POLLHUP | POLLNVAL,
        revents: 0,
    };
    loop {
        let ret = unsafe { poll(&mut fd, 1, 0) };
        if ret > 0 {
            return Some(fd.revents);
        }
        if ret == 0 {
            return None;
        }
        if errno() == EINTR {
            continue;
        }
        return Some(POLLERR);
    }
}

fn drain_output_message(
    output_fd: c_int,
    agent_fd: &Arc<Mutex<c_int>>,
    stderr: bool,
    channel_id: u64,
    buf: &mut [u8],
) -> bool {
    let mut saw_eof = false;
    loop {
        let Some(revents) = poll_output(output_fd) else {
            break;
        };
        if revents & (POLLIN | POLLERR | POLLHUP | POLLNVAL) == 0 {
            break;
        }
        let n = unsafe { read(output_fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
        if n > 0 {
            let bytes = buf[..n as usize].to_vec();
            let message = if stderr {
                Message::Stderr { channel_id, bytes }
            } else {
                Message::Data { channel_id, bytes }
            };
            let _ = write_message_locked(agent_fd, &message);
            continue;
        }
        if n < 0 && errno() == EINTR {
            continue;
        }
        if n < 0 && errno() == EAGAIN {
            break;
        }
        if n < 0 && errno() == EIO {
            saw_eof = true;
            break;
        }
        saw_eof = true;
        break;
    }
    saw_eof
}

fn run_channel_pty(
    agent_fd: Arc<Mutex<c_int>>,
    channel_id: u64,
    argv: Vec<String>,
    cwd: String,
    env: Vec<(String, String)>,
    term: String,
    colorterm: String,
    rows: u16,
    cols: u16,
    uid: u32,
    gid: u32,
    group: String,
    rx: mpsc::Receiver<ChannelInput>,
) {
    ensure_exec_user(uid, gid, &group);
    relax_nested_kvm();
    let argv = resolve_login_shell(argv, uid);
    let mut pty_master = -1;
    let mut pty_slave = -1;
    let control_socket = channel_control_socket(channel_id);
    if unsafe {
        openpty(
            &mut pty_master,
            &mut pty_slave,
            ptr::null_mut(),
            ptr::null(),
            ptr::null(),
        )
    } < 0
    {
        let _ = write_message_locked(
            &agent_fd,
            &Message::Error {
                channel_id,
                message: "openpty failed".to_string(),
            },
        );
        send_status(&agent_fd, channel_id, 127 << 8);
        return;
    }
    set_cloexec(pty_master);
    set_cloexec(pty_slave);
    let winsize = Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        ioctl(pty_master, TIOCSWINSZ, &winsize as *const Winsize);
    }

    let pid = unsafe { fork() };
    if pid < 0 {
        unsafe {
            close(pty_master);
            close(pty_slave);
        }
        send_status(&agent_fd, channel_id, 127 << 8);
        return;
    }
    if pid == 0 {
        unsafe {
            close(pty_master);
            setsid();
            ioctl(pty_slave, TIOCSCTTY, 0);
            dup2(pty_slave, STDIN_FILENO);
            dup2(pty_slave, STDOUT_FILENO);
            dup2(pty_slave, STDERR_FILENO);
            if pty_slave > STDERR_FILENO {
                close(pty_slave);
            }
        }
        if !cwd.is_empty() {
            if let Ok(cwd) = CString::new(cwd) {
                unsafe {
                    chdir(cwd.as_ptr());
                }
            }
        }
        if let (Ok(name), Ok(value)) = (CString::new("TERM"), CString::new(term)) {
            unsafe {
                setenv(name.as_ptr(), value.as_ptr(), 1);
            }
        }
        if !colorterm.is_empty() {
            if let (Ok(name), Ok(value)) = (CString::new("COLORTERM"), CString::new(colorterm)) {
                unsafe {
                    setenv(name.as_ptr(), value.as_ptr(), 1);
                }
            }
        }
        set_default_exec_environment();
        set_forwarded_environment(&env);
        set_lnx_request_id(channel_id);
        set_lnx_control_socket(&control_socket);
        drop_to_exec_user(uid, gid);
        let (_storage, ptrs) = make_argv(&argv);
        unsafe {
            execvp(ptrs[0], ptrs.as_ptr());
            exec_failed(ptrs[0]);
        }
    }
    unsafe {
        close(pty_slave);
    }
    let _ = write_message_locked(&agent_fd, &Message::ExecStarted { channel_id });
    set_nonblocking(pty_master);
    let control_fd = listen_unix(&control_socket);
    let mut pending_control_fd = -1;
    let mut buf = [0u8; 8192];
    let mut status = 0;
    let mut child_exited = false;
    let mut pty_eof = false;
    loop {
        let mut pollfds = [
            PollFd {
                fd: pty_master,
                events: POLLIN | POLLHUP | POLLERR | POLLNVAL,
                revents: 0,
            },
            PollFd {
                fd: control_fd,
                events: POLLIN | POLLHUP | POLLERR | POLLNVAL,
                revents: 0,
            },
        ];
        let n = unsafe { poll(pollfds.as_mut_ptr(), 2, 25) };
        if n > 0 && pollfds[0].revents & (POLLIN | POLLHUP | POLLERR | POLLNVAL) != 0 {
            pty_eof |= drain_output_message(pty_master, &agent_fd, false, channel_id, &mut buf);
        }
        if n > 0 && pollfds[1].revents & POLLIN != 0 && pending_control_fd < 0 {
            if let Some(request) = accept_channel_control(control_fd) {
                handle_channel_control(request, &mut pending_control_fd, &agent_fd, channel_id);
            }
        }
        while let Ok(input) = rx.try_recv() {
            match input {
                ChannelInput::Data(bytes) => {
                    let _ = write_all(pty_master, &bytes);
                }
                ChannelInput::Eof => {
                    let _ = write_all(pty_master, &[0x04]);
                }
                ChannelInput::Resize(rows, cols) => {
                    let winsize = Winsize {
                        ws_row: rows,
                        ws_col: cols,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    };
                    unsafe {
                        ioctl(pty_master, TIOCSWINSZ, &winsize as *const Winsize);
                    }
                }
                ChannelInput::Close => {
                    unsafe {
                        kill(pid, SIGTERM);
                    }
                    status = 130 << 8;
                    break;
                }
                ChannelInput::SnapshotComplete | ChannelInput::OpenUrlComplete => {
                    complete_pending_control(&mut pending_control_fd, true);
                }
                ChannelInput::SnapshotFailed | ChannelInput::OpenUrlFailed => {
                    complete_pending_control(&mut pending_control_fd, false);
                }
            }
        }
        if !child_exited {
            let waited = unsafe { waitpid(pid, &mut status, WNOHANG) };
            if waited == pid || (waited < 0 && errno() == ECHILD) {
                child_exited = true;
                log!(
                    "channel.pipe.child_exited channel={channel_id:016x} waited={waited} status={}",
                    exit_status(status)
                );
            } else if waited < 0 {
                status = 127 << 8;
                child_exited = true;
                log!(
                    "channel.pipe.wait_error channel={channel_id:016x} errno={}",
                    errno()
                );
            }
        }
        if child_exited {
            pty_eof |= drain_output_message(pty_master, &agent_fd, false, channel_id, &mut buf);
        }
        if child_exited && pty_eof {
            break;
        }
    }
    unsafe {
        close(pty_master);
    }
    close_control_fd(pending_control_fd);
    close_channel_control_socket(control_fd, &control_socket);
    send_status(&agent_fd, channel_id, status);
}

fn run_channel_pipe(
    agent_fd: Arc<Mutex<c_int>>,
    channel_id: u64,
    argv: Vec<String>,
    cwd: String,
    env: Vec<(String, String)>,
    uid: u32,
    gid: u32,
    group: String,
    eof_requested: Arc<AtomicBool>,
    rx: mpsc::Receiver<ChannelInput>,
) {
    ensure_exec_user(uid, gid, &group);
    relax_nested_kvm();
    let argv = resolve_login_shell(argv, uid);
    let control_socket = channel_control_socket(channel_id);
    let child_exec = make_child_exec(&argv, &cwd, &env, channel_id, &control_socket);
    let mut stdin_pipe = [-1; 2];
    let mut stdout_pipe = [-1; 2];
    let mut stderr_pipe = [-1; 2];
    if !make_pipe(&mut stdin_pipe) || !make_pipe(&mut stdout_pipe) || !make_pipe(&mut stderr_pipe) {
        close_pipe(&mut stdin_pipe);
        close_pipe(&mut stdout_pipe);
        close_pipe(&mut stderr_pipe);
        send_status(&agent_fd, channel_id, 127 << 8);
        return;
    }
    let pid = unsafe { fork() };
    if pid < 0 {
        close_pipe(&mut stdin_pipe);
        close_pipe(&mut stdout_pipe);
        close_pipe(&mut stderr_pipe);
        send_status(&agent_fd, channel_id, 127 << 8);
        return;
    }
    if pid == 0 {
        unsafe {
            close(stdin_pipe[1]);
            close(stdout_pipe[0]);
            close(stderr_pipe[0]);
            dup2(stdin_pipe[0], STDIN_FILENO);
            dup2(stdout_pipe[1], STDOUT_FILENO);
            dup2(stderr_pipe[1], STDERR_FILENO);
            if stdin_pipe[0] > STDERR_FILENO {
                close(stdin_pipe[0]);
            }
            if stdout_pipe[1] > STDERR_FILENO {
                close(stdout_pipe[1]);
            }
            if stderr_pipe[1] > STDERR_FILENO {
                close(stderr_pipe[1]);
            }
        }
        if let Some(cwd) = child_exec.cwd.as_ref() {
            unsafe {
                if chdir(cwd.as_ptr()) < 0 {
                    write_all(STDERR_FILENO, b"chdir failed errno=");
                    write_decimal(STDERR_FILENO, errno() as u64);
                    write_all(STDERR_FILENO, b" cwd=");
                    write_all(STDERR_FILENO, cwd.as_bytes());
                    write_all(
                        STDERR_FILENO,
                        b"\nlnx: working directory is not visible inside the VM; if this is a host-shared path, run: lnx fs unshare ",
                    );
                    write_all(STDERR_FILENO, cwd.as_bytes());
                    child_die(b"\n");
                }
            }
        }
        drop_to_exec_user_child(uid, gid);
        for path in &child_exec.exec_paths {
            unsafe {
                execve(
                    path.as_ptr(),
                    child_exec.argv_ptrs.as_ptr(),
                    child_exec.env_ptrs.as_ptr(),
                );
            }
        }
        exec_failed(child_exec.argv_ptrs[0]);
    }
    log!("channel.pipe.spawned channel={channel_id:016x} pid={pid}");
    let _ = write_message_locked(&agent_fd, &Message::ExecStarted { channel_id });
    unsafe {
        close(stdin_pipe[0]);
        close(stdout_pipe[1]);
        close(stderr_pipe[1]);
    }
    let mut stdin_write = stdin_pipe[1];
    let stdout_read = stdout_pipe[0];
    let stderr_read = stderr_pipe[0];
    if !set_nonblocking(stdout_read) || !set_nonblocking(stderr_read) {
        log!(
            "channel.pipe.nonblocking_failed channel={channel_id:016x} errno={}",
            errno()
        );
    }
    let control_fd = listen_unix(&control_socket);
    let mut pending_control_fd = -1;
    let mut buf = [0u8; 8192];
    let mut status = 0;
    let mut child_exited = false;
    let mut child_probe_logged = false;
    let mut loop_count = 0u32;
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    loop {
        loop_count = loop_count.saturating_add(1);
        let mut pollfds = [
            PollFd {
                fd: stdout_read,
                events: POLLIN | POLLHUP | POLLERR | POLLNVAL,
                revents: 0,
            },
            PollFd {
                fd: stderr_read,
                events: POLLIN | POLLHUP | POLLERR | POLLNVAL,
                revents: 0,
            },
            PollFd {
                fd: control_fd,
                events: POLLIN | POLLHUP | POLLERR | POLLNVAL,
                revents: 0,
            },
        ];
        let n = unsafe { poll(pollfds.as_mut_ptr(), 3, 25) };
        if n > 0 && pollfds[0].revents & (POLLIN | POLLHUP | POLLERR | POLLNVAL) != 0 {
            stdout_eof |= drain_output_message(stdout_read, &agent_fd, false, channel_id, &mut buf);
        }
        if n > 0 && pollfds[1].revents & (POLLIN | POLLHUP | POLLERR | POLLNVAL) != 0 {
            stderr_eof |= drain_output_message(stderr_read, &agent_fd, true, channel_id, &mut buf);
        }
        if n > 0 && pollfds[2].revents & POLLIN != 0 && pending_control_fd < 0 {
            if let Some(request) = accept_channel_control(control_fd) {
                handle_channel_control(request, &mut pending_control_fd, &agent_fd, channel_id);
            }
        }
        while let Ok(input) = rx.try_recv() {
            match input {
                ChannelInput::Data(bytes) if stdin_write >= 0 => {
                    if !write_all(stdin_write, &bytes) {
                        log!(
                            "channel.pipe.stdin.write_failed channel={channel_id:016x} errno={}",
                            errno()
                        );
                        unsafe {
                            close(stdin_write);
                        }
                        stdin_write = -1;
                    }
                }
                ChannelInput::Eof if stdin_write >= 0 => {
                    unsafe {
                        close(stdin_write);
                    }
                    stdin_write = -1;
                }
                ChannelInput::Close => {
                    unsafe {
                        kill(pid, SIGTERM);
                    }
                    status = 130 << 8;
                    break;
                }
                ChannelInput::SnapshotComplete | ChannelInput::OpenUrlComplete => {
                    complete_pending_control(&mut pending_control_fd, true);
                }
                ChannelInput::SnapshotFailed | ChannelInput::OpenUrlFailed => {
                    complete_pending_control(&mut pending_control_fd, false);
                }
                _ => {}
            }
        }
        if eof_requested.swap(false, Ordering::SeqCst) && stdin_write >= 0 {
            log!("channel.pipe.stdin.eof_latched channel={channel_id:016x}");
            unsafe {
                close(stdin_write);
            }
            stdin_write = -1;
        }
        if !child_exited {
            let waited = unsafe { waitpid(pid, &mut status, WNOHANG) };
            if waited == pid || (waited < 0 && errno() == ECHILD) {
                child_exited = true;
            } else if waited < 0 {
                status = 127 << 8;
                child_exited = true;
            }
        }
        if !child_exited && !child_probe_logged && loop_count >= 80 {
            log_child_probe(channel_id, pid);
            child_probe_logged = true;
        }
        if child_exited {
            if stdin_write >= 0 {
                unsafe {
                    close(stdin_write);
                }
                stdin_write = -1;
            }
            stdout_eof |= drain_output_message(stdout_read, &agent_fd, false, channel_id, &mut buf);
            stderr_eof |= drain_output_message(stderr_read, &agent_fd, true, channel_id, &mut buf);
        }
        if child_exited && stdout_eof && stderr_eof {
            break;
        }
    }
    unsafe {
        if stdin_write >= 0 {
            close(stdin_write);
        }
        close(stdout_read);
        close(stderr_read);
    }
    close_control_fd(pending_control_fd);
    close_channel_control_socket(control_fd, &control_socket);
    send_status(&agent_fd, channel_id, status);
}

fn run_channel_tcp(
    agent_fd: Arc<Mutex<c_int>>,
    channel_id: u64,
    host: String,
    port: u16,
    rx: mpsc::Receiver<ChannelInput>,
) {
    let stream = match TcpStream::connect((host.as_str(), port)) {
        Ok(stream) => stream,
        Err(e) => {
            let _ = write_message_locked(
                &agent_fd,
                &Message::Error {
                    channel_id,
                    message: format!("connect {host}:{port}: {e}"),
                },
            );
            let _ = write_message_locked(&agent_fd, &Message::Close { channel_id });
            return;
        }
    };
    let mut reader = match stream.try_clone() {
        Ok(reader) => reader,
        Err(e) => {
            let _ = write_message_locked(
                &agent_fd,
                &Message::Error {
                    channel_id,
                    message: format!("clone tcp stream: {e}"),
                },
            );
            let _ = write_message_locked(&agent_fd, &Message::Close { channel_id });
            return;
        }
    };
    let reader_agent_fd = Arc::clone(&agent_fd);
    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    let _ = write_message_locked(&reader_agent_fd, &Message::Eof { channel_id });
                    break;
                }
                Ok(n) => {
                    let _ = write_message_locked(
                        &reader_agent_fd,
                        &Message::Data {
                            channel_id,
                            bytes: buf[..n].to_vec(),
                        },
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    let _ = write_message_locked(
                        &reader_agent_fd,
                        &Message::Error {
                            channel_id,
                            message: format!("tcp read: {e}"),
                        },
                    );
                    break;
                }
            }
        }
        let _ = done_tx.send(());
    });

    let mut writer = stream;
    loop {
        if done_rx.try_recv().is_ok() {
            break;
        }
        match rx.recv_timeout(std::time::Duration::from_millis(25)) {
            Ok(ChannelInput::Data(bytes)) => {
                if let Err(e) = writer.write_all(&bytes) {
                    let _ = write_message_locked(
                        &agent_fd,
                        &Message::Error {
                            channel_id,
                            message: format!("tcp write: {e}"),
                        },
                    );
                    break;
                }
            }
            Ok(ChannelInput::Eof) => {
                let _ = writer.shutdown(Shutdown::Write);
            }
            Ok(ChannelInput::Resize(_, _))
            | Ok(ChannelInput::SnapshotComplete)
            | Ok(ChannelInput::SnapshotFailed)
            | Ok(ChannelInput::OpenUrlComplete)
            | Ok(ChannelInput::OpenUrlFailed) => {}
            Ok(ChannelInput::Close) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = writer.shutdown(Shutdown::Both);
    let _ = write_message_locked(&agent_fd, &Message::Close { channel_id });
}

fn agent_loop() {
    log!("agent.loop.start");
    let mut fd = connect_vsock(AGENT_PORT);
    log!("agent.loop.connected fd={fd}");
    let agent_fd = Arc::new(Mutex::new(fd));
    let _ = publish_agent_fd_and_hello(&agent_fd, fd);
    start_listener_monitor(Arc::clone(&agent_fd));
    let mut channels: Vec<(u64, ChannelState)> = Vec::new();
    loop {
        let message = read_message(fd);
        let Some(message) = message else {
            log!("agent.loop.read_closed fd={fd}; reconnecting");
            unsafe {
                close(fd);
            }
            fd = reconnect_after_snapshot_point();
            log!("agent.loop.reconnected fd={fd}");
            let _ = publish_agent_fd_and_hello(&agent_fd, fd);
            continue;
        };
        match message {
            Message::Hello { version } if version == PROTOCOL_VERSION => {}
            Message::OpenExec {
                channel_id,
                argv,
                cwd,
                pty,
                term,
                colorterm,
                rows,
                cols,
                uid,
                gid,
                group,
                env,
            } => {
                let (tx, rx) = mpsc::channel();
                let eof_requested = Arc::new(AtomicBool::new(false));
                channels.push((
                    channel_id,
                    ChannelState {
                        tx,
                        eof_requested: Arc::clone(&eof_requested),
                    },
                ));
                let writer = Arc::clone(&agent_fd);
                if pty {
                    thread::spawn(move || {
                        run_channel_pty(
                            writer, channel_id, argv, cwd, env, term, colorterm, rows, cols, uid,
                            gid, group, rx,
                        )
                    });
                } else {
                    thread::spawn(move || {
                        run_channel_pipe(
                            writer,
                            channel_id,
                            argv,
                            cwd,
                            env,
                            uid,
                            gid,
                            group,
                            eof_requested,
                            rx,
                        )
                    });
                }
            }
            Message::OpenTcp {
                channel_id,
                host,
                port,
            } => {
                let (tx, rx) = mpsc::channel();
                channels.push((
                    channel_id,
                    ChannelState {
                        tx,
                        eof_requested: Arc::new(AtomicBool::new(false)),
                    },
                ));
                let writer = Arc::clone(&agent_fd);
                thread::spawn(move || run_channel_tcp(writer, channel_id, host, port, rx));
            }
            Message::Data { channel_id, bytes } => {
                if let Some((_, state)) = channels.iter().find(|(id, _)| *id == channel_id) {
                    if state.tx.send(ChannelInput::Data(bytes)).is_err() {
                        log!("channel.data.send_failed channel={channel_id:016x}");
                    }
                } else {
                    log!("channel.data.no_channel channel={channel_id:016x}");
                }
            }
            Message::Eof { channel_id } => {
                if let Some((_, state)) = channels.iter().find(|(id, _)| *id == channel_id) {
                    state.eof_requested.store(true, Ordering::SeqCst);
                    if state.tx.send(ChannelInput::Eof).is_err() {
                        log!("channel.eof.send_failed channel={channel_id:016x}");
                    }
                } else {
                    log!("channel.eof.no_channel channel={channel_id:016x}");
                }
            }
            Message::WindowResize {
                channel_id,
                rows,
                cols,
            } => {
                if let Some((_, state)) = channels.iter().find(|(id, _)| *id == channel_id) {
                    let _ = state.tx.send(ChannelInput::Resize(rows, cols));
                }
            }
            Message::Close { channel_id } => {
                if let Some((_, state)) = channels.iter().find(|(id, _)| *id == channel_id) {
                    let _ = state.tx.send(ChannelInput::Close);
                }
                channels.retain(|(id, _)| *id != channel_id);
            }
            Message::CheckpointCreated { channel_id } => {
                if let Some((_, state)) = channels.iter().find(|(id, _)| *id == channel_id) {
                    let _ = state.tx.send(ChannelInput::SnapshotComplete);
                }
            }
            Message::Error { channel_id, .. } => {
                if let Some((_, state)) = channels.iter().find(|(id, _)| *id == channel_id) {
                    let _ = state.tx.send(ChannelInput::SnapshotFailed);
                }
            }
            Message::ExecStarted { .. } => {}
            Message::OpenUrlResult { channel_id, ok } => {
                if let Some((_, state)) = channels.iter().find(|(id, _)| *id == channel_id) {
                    let input = if ok {
                        ChannelInput::OpenUrlComplete
                    } else {
                        ChannelInput::OpenUrlFailed
                    };
                    let _ = state.tx.send(input);
                }
            }
            Message::RestoreSync {
                channel_id,
                entropy,
            } => match restore_sync_guest_caches(&entropy) {
                Ok(()) => {
                    let _ = write_message_locked(&agent_fd, &Message::RestoreSynced { channel_id });
                }
                Err(message) => {
                    let _ = write_message_locked(
                        &agent_fd,
                        &Message::Error {
                            channel_id,
                            message,
                        },
                    );
                }
            },
            Message::ExitStatus { channel_id, .. } => {
                channels.retain(|(id, _)| *id != channel_id);
            }
            Message::SnapshotReady => {
                unsafe {
                    sync();
                    close(fd);
                }
                fd = request_snapshot_and_reconnect();
                if let Ok(mut shared) = agent_fd.lock() {
                    *shared = fd;
                }
                let _ = write_message_locked(
                    &agent_fd,
                    &Message::Hello {
                        version: PROTOCOL_VERSION,
                    },
                );
            }
            _ => {}
        }
    }
}

pub fn main() {
    let args = env::args().collect::<Vec<_>>();
    if args
        .first()
        .map(|arg| arg.ends_with("/xdg-open") || arg == "xdg-open")
        .unwrap_or(false)
    {
        xdg_open_mode(&args);
    }
    if args
        .first()
        .map(|arg| arg.ends_with("/lnxctl") || arg == "lnxctl")
        .unwrap_or(false)
    {
        lnxctl_mode(&args);
    }
    if args.get(1).map(String::as_str) == Some("--init")
        || args
            .first()
            .map(|arg| arg == "/init" || arg == "init")
            .unwrap_or(false)
    {
        init_mode();
    }
    if args.len() != 3 || args[1] != "--agent" {
        unsafe { _exit(125) }
    }
    let _port = args[2].parse::<u32>().unwrap_or(AGENT_PORT);
    agent_loop();
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_BROWSER, DEFAULT_PATH, EXEC_HOME, make_child_exec, parse_proc_net_tcp_listener,
    };

    #[test]
    fn proc_net_tcp_listener_parser_keeps_loopback_and_wildcard_high_ports() {
        assert_eq!(
            parse_proc_net_tcp_listener(
                "  0: 0100007F:0EBB 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000 0 1 1 0000000000000000 100 0 0 10 0",
                false,
            ),
            Some(3771)
        );
        assert_eq!(
            parse_proc_net_tcp_listener(
                "  0: 00000000:1455 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000 0 1 1 0000000000000000 100 0 0 10 0",
                false,
            ),
            Some(5205)
        );
    }

    #[test]
    fn proc_net_tcp_listener_parser_rejects_low_ports_and_non_listeners() {
        assert_eq!(
            parse_proc_net_tcp_listener(
                "  0: 0100007F:0050 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000 0 1 1 0000000000000000 100 0 0 10 0",
                false,
            ),
            None
        );
        assert_eq!(
            parse_proc_net_tcp_listener(
                "  0: 0100007F:0EBB 00000000:0000 01 00000000:00000000 00:00000000 00000000  1000 0 1 1 0000000000000000 100 0 0 10 0",
                false,
            ),
            None
        );
    }

    #[test]
    fn proc_net_tcp6_listener_parser_keeps_loopback_and_wildcard() {
        assert_eq!(
            parse_proc_net_tcp_listener(
                "  0: 00000000000000000000000000000000:1455 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000 1000 0 1 1 0000000000000000 100 0 0 10 0",
                true,
            ),
            Some(5205)
        );
        assert_eq!(
            parse_proc_net_tcp_listener(
                "  0: 00000000000000000000000001000000:0EBB 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000 1000 0 1 1 0000000000000000 100 0 0 10 0",
                true,
            ),
            Some(3771)
        );
    }

    #[test]
    fn child_exec_sets_browser_to_guest_xdg_open_shim() {
        let child = make_child_exec(
            &["env".to_string()],
            "/tmp",
            &[],
            0x1234,
            "/run/lnx-agent.sock",
        );
        let browser = child
            .env_storage
            .iter()
            .map(|entry| entry.to_str().expect("env entry is utf-8"))
            .find_map(|entry| entry.strip_prefix("BROWSER="))
            .expect("BROWSER is set");

        assert_eq!(browser, DEFAULT_BROWSER);
    }

    #[test]
    fn default_path_starts_with_exec_user_tool_bins() {
        assert!(DEFAULT_PATH.starts_with(&format!(
            "{EXEC_HOME}/.local/bin:{EXEC_HOME}/go/bin:{EXEC_HOME}/.cargo/bin:"
        )));
    }
}
