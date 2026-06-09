use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void, CStr, CString};
use std::io::{Error, Read, Write};
use std::mem::size_of;
use std::net::{Shutdown, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::ptr;
use std::sync::{mpsc, Arc, Mutex};
use std::{env, fs, thread};

use crate::protocol::{Message, MAX_MESSAGE_SIZE, PROTOCOL_VERSION};

const AF_VSOCK: c_int = 40;
const AF_UNIX: c_int = 1;
const SOCK_STREAM: c_int = 1;
const VMADDR_CID_HOST: u32 = 2;
const AGENT_PORT: u32 = 10240;
const SNAPSHOT_PORT: u32 = 10241;
const CONTROL_PORT: u32 = 10242;
const EINTR: c_int = 4;
const EAGAIN: c_int = 11;
const ECHILD: c_int = 10;
const EIO: c_int = 5;
const F_GETFL: c_int = 3;
const F_SETFL: c_int = 4;
const O_NONBLOCK: c_int = 0o4000;
const POLLIN: i16 = 0x0001;
const POLLERR: i16 = 0x0008;
const POLLHUP: i16 = 0x0010;
const STDIN_FILENO: c_int = 0;
const STDOUT_FILENO: c_int = 1;
const STDERR_FILENO: c_int = 2;
const SIGTERM: c_int = 15;
const WNOHANG: c_int = 1;
const TIOCSCTTY: c_ulong = 0x540e;
const TIOCSWINSZ: c_ulong = 0x5414;
const CLOCK_REALTIME: c_int = 0;
const MS_BIND: c_ulong = 4096;
const MS_REC: c_ulong = 16384;
const MS_PRIVATE: c_ulong = 262144;
const MNT_DETACH: c_int = 2;
const SYS_PIVOT_ROOT: isize = 41;
const POLLOUT: i16 = 0x0004;

const FRAME_OUTPUT: u8 = b'O';
const FRAME_STDERR: u8 = b'E';
const FRAME_INPUT: u8 = b'I';
const FRAME_STATUS: u8 = b'S';
const FRAME_SNAPSHOT: u8 = b'K';
const FRAME_CONTROL_SNAPSHOT_EXIT: u8 = b'X';
const FRAME_CONTROL_OK: u8 = b'x';
const REQUEST_FLAG_PTY: u8 = 1;
const DEFAULT_PATH: &str =
    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/snap/bin";
const AGENT_PATH: &str = "/run/lnx/lnx-agent";
const LNXCTL_PATH: &str = "/run/lnx/lnxctl";
const OLD_AGENT_PATH: &str = "/usr/local/lib/lnx/lnx-agent";
const OLD_LNXCTL_PATH: &str = "/usr/local/bin/lnxctl";
const OLD_SERVICE_PATH: &str = "/etc/systemd/system/lnx-agent.service";
const OLD_WANTS_LINK: &str = "/etc/systemd/system/multi-user.target.wants/lnx-agent.service";
const CONTROL_SOCKET: &str = "/run/lnx-agent.sock";
const CONTROL_SOCKET_ENV: &str = "LNX_CONTROL_SOCKET";
const SERVICE_PATH: &str = "/run/systemd/system/lnx-agent.service";
const WANTS_DIR: &str = "/run/systemd/system/multi-user.target.wants";
const WANTS_LINK: &str = "/run/systemd/system/multi-user.target.wants/lnx-agent.service";
const WANTS_LINK_C: &[u8] = b"/newroot/run/systemd/system/multi-user.target.wants/lnx-agent.service\0";

enum ChannelInput {
    Data(Vec<u8>),
    Eof,
    Resize(u16, u16),
    Close,
    SnapshotComplete,
    SnapshotFailed,
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
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[repr(C)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

unsafe extern "C" {
    fn accept(fd: c_int, addr: *mut Sockaddr, len: *mut c_uint) -> c_int;
    fn bind(fd: c_int, addr: *const Sockaddr, len: c_uint) -> c_int;
    fn chdir(path: *const c_char) -> c_int;
    fn chmod(path: *const c_char, mode: c_uint) -> c_int;
    fn clock_settime(clockid: c_int, tp: *const Timespec) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn connect(fd: c_int, addr: *const Sockaddr, len: c_uint) -> c_int;
    fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
    fn execvp(file: *const c_char, argv: *const *const c_char) -> c_int;
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    fn fork() -> c_int;
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
    fn mount(
        source: *const c_char,
        target: *const c_char,
        filesystemtype: *const c_char,
        mountflags: c_ulong,
        data: *const c_void,
    ) -> c_int;
    fn pipe(fds: *mut c_int) -> c_int;
    fn poll(fds: *mut PollFd, nfds: c_ulong, timeout: c_int) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    fn socket(domain: c_int, typ: c_int, protocol: c_int) -> c_int;
    fn setsid() -> c_int;
    fn setenv(name: *const c_char, value: *const c_char, overwrite: c_int) -> c_int;
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

fn die(msg: &str) -> ! {
    log(&format!("{msg}: {}", Error::last_os_error()));
    unsafe { _exit(125) }
}

fn log(msg: &str) {
    write_all(STDERR_FILENO, b"lnx-agent: ");
    write_all(STDERR_FILENO, msg.as_bytes());
    write_all(STDERR_FILENO, b"\n");
    let _ = fs::write("/dev/kmsg", format!("lnx-agent: {msg}\n"));
}

fn cstr(bytes: &'static [u8]) -> *const c_char {
    bytes.as_ptr() as *const c_char
}

fn mount_fs(source: &'static [u8], target: &'static [u8], fstype: &'static [u8], flags: c_ulong, data: &'static [u8]) {
    let data_ptr = if data.is_empty() {
        ptr::null()
    } else {
        cstr(data) as *const c_void
    };
    if unsafe { mount(cstr(source), cstr(target), cstr(fstype), flags, data_ptr) } < 0 {
        die("mount");
    }
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

fn run_status(args: &[&str]) -> c_int {
    let c_args = args
        .iter()
        .map(|arg| std::ffi::CString::new(*arg).unwrap())
        .collect::<Vec<_>>();
    let mut argv = c_args.iter().map(|arg| arg.as_ptr()).collect::<Vec<_>>();
    argv.push(ptr::null());

    let pid = unsafe { fork() };
    if pid < 0 {
        return -1;
    }
    if pid == 0 {
        unsafe {
            execvp(argv[0], argv.as_ptr());
            _exit(127);
        }
    }

    let mut status = 0;
    loop {
        let waited = unsafe { waitpid(pid, &mut status, 0) };
        if waited == pid {
            return status;
        }
        if waited < 0 && errno() != EINTR {
            return -1;
        }
    }
}

fn configure_network() {
    let _ = run_status(&["/sbin/ip", "link", "set", "lo", "up"]);
    let _ = run_status(&["/sbin/ip", "link", "set", "eth0", "up"]);
    let _ = run_status(&[
        "/sbin/ip",
        "addr",
        "replace",
        "192.168.127.2/24",
        "dev",
        "eth0",
    ]);
    let _ = run_status(&[
        "/sbin/ip",
        "route",
        "replace",
        "default",
        "via",
        "192.168.127.1",
        "dev",
        "eth0",
    ]);
    let _ = fs::remove_file("/etc/resolv.conf");
    let _ = fs::write(
        "/etc/resolv.conf",
        "nameserver 192.168.127.1\nnameserver 1.1.1.1\n",
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
    if !wait_for_path("/dev/pmem0") {
        log("timed out waiting for /dev/pmem0");
    }
    mount_fs(
        b"/dev/pmem0\0",
        b"/newroot\0",
        b"ext4\0",
        0,
        b"errors=continue,dax\0",
    );

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
    ensure_dir(&format!("/newroot{WANTS_DIR}"));

    for (source, target) in [
        ("/lnx-agent", format!("/newroot{AGENT_PATH}")),
        ("/lnxctl", format!("/newroot{LNXCTL_PATH}")),
    ] {
        if let Err(e) = fs::write(&target, []) {
            log(&format!("create bind target {target}: {e}"));
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
            log(&format!("chmod bind target {target}: {e}"));
            unsafe { _exit(125) }
        }
    }
    let lnxctl_link = CString::new(format!("/newroot{OLD_LNXCTL_PATH}")).unwrap();
    let _ = unsafe { symlink(cstr(b"/run/lnx/lnxctl\0"), lnxctl_link.as_ptr()) };

    let unit = "[Unit]\n\
Description=lnx guest agent\n\
After=basic.target\n\
Before=multi-user.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart=/run/lnx/lnx-agent --agent 10240\n\
Environment=LNX_CONTROL_SOCKET=/run/lnx-agent.sock\n\
StandardOutput=journal+console\n\
StandardError=journal+console\n\
Restart=always\n\
RestartSec=1\n\
\n\
[Install]\n\
WantedBy=multi-user.target\n";
    if let Err(e) = fs::write(format!("/newroot{SERVICE_PATH}"), unit) {
        log(&format!("write lnx-agent.service: {e}"));
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
    if unsafe { syscall(SYS_PIVOT_ROOT, cstr(b"/newroot\0"), cstr(b"/newroot/oldroot\0")) } < 0 {
        die("pivot_root");
    }
    if unsafe { chdir(cstr(b"/\0")) } < 0 {
        die("chdir");
    }
    let _ = unsafe { umount2(cstr(b"/oldroot\0"), MNT_DETACH) };
    let _ = fs::remove_dir("/oldroot");

    configure_network();

    let init = cstr(b"/sbin/init\0");
    let argv = [init, ptr::null()];
    unsafe {
        execvp(init, argv.as_ptr());
    }
    die("exec /sbin/init")
}

fn connect_vsock(port: u32) -> c_int {
    for _ in 0..600 {
        let addr = vsock_addr(port);
        let fd = unsafe { socket(AF_VSOCK, SOCK_STREAM, 0) };
        if fd < 0 {
            die("socket(AF_VSOCK)");
        }
        let ret = unsafe {
            connect(
                fd,
                &addr as *const SockaddrVm as *const Sockaddr,
                size_of::<SockaddrVm>() as c_uint,
            )
        };
        if ret == 0 {
            return fd;
        }
        unsafe {
            close(fd);
        }
        unsafe {
            usleep(100_000);
        }
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

fn write_command_frame(fd: c_int, frame_type: u8, request_id: u64, payload: &[u8]) -> bool {
    write_all(fd, &[frame_type])
        && write_all(fd, &((payload.len() + 8) as u32).to_be_bytes())
        && write_all(fd, &request_id.to_be_bytes())
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

fn snapshot_resume_wait(fd: c_int) {
    let _ = write_all(fd, b"R");
    let mut buf = [0u8; 1];
    loop {
        let n = unsafe { read(fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
        if n < 0 && errno() == EINTR {
            continue;
        }
        break;
    }
}

fn request_snapshot_and_reconnect() -> c_int {
    let fd = connect_vsock(SNAPSHOT_PORT);
    request_snapshot(fd);
    snapshot_resume_wait(fd);
    let agent_fd = reconnect_after_snapshot_point();
    unsafe {
        close(fd);
    }
    agent_fd
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
            return false;
        }
        if n == 0 {
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

fn read_u8(fd: c_int) -> u8 {
    let mut buf = [0u8; 1];
    read_exact(fd, &mut buf);
    buf[0]
}

fn read_u16(fd: c_int) -> u16 {
    let mut buf = [0u8; 2];
    read_exact(fd, &mut buf);
    u16::from_be_bytes(buf)
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

fn read_string(fd: c_int, max_len: usize, label: &str) -> String {
    let len = read_u32(fd) as usize;
    if len > max_len {
        die(label);
    }
    let mut bytes = vec![0u8; len];
    if len > 0 {
        read_exact(fd, &mut bytes);
    }
    String::from_utf8(bytes).unwrap_or_default()
}

fn set_nonblocking(fd: c_int) {
    let flags = unsafe { fcntl(fd, F_GETFL) };
    if flags >= 0 {
        unsafe {
            fcntl(fd, F_SETFL, flags | O_NONBLOCK);
        }
    }
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

fn reconnect_agent_fd(agent_fd: &mut c_int) {
    if *agent_fd >= 0 {
        unsafe {
            close(*agent_fd);
        }
    }
    *agent_fd = reconnect_after_snapshot_point();
}

fn write_command_frame_reconnect(
    agent_fd: &mut c_int,
    frame_type: u8,
    request_id: u64,
    payload: &[u8],
) {
    if write_command_frame(*agent_fd, frame_type, request_id, payload) {
        return;
    }
    reconnect_agent_fd(agent_fd);
    let _ = write_command_frame(*agent_fd, frame_type, request_id, payload);
}

fn drain_output(
    output_fd: c_int,
    agent_fd: &mut c_int,
    frame_type: u8,
    request_id: u64,
    buf: &mut [u8],
) -> bool {
    let mut saw_eof = false;
    loop {
        let n = unsafe { read(output_fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
        if n > 0 {
            write_command_frame_reconnect(agent_fd, frame_type, request_id, &buf[..n as usize]);
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

enum InputFrame {
    Data(Vec<u8>),
    Eof,
    Stale,
}

fn read_input_frame(agent_fd: c_int, request_id: u64) -> Option<InputFrame> {
    let mut frame_type = [0u8; 1];
    if !try_read_exact(agent_fd, &mut frame_type) {
        return None;
    }
    let mut len_buf = [0u8; 4];
    if !try_read_exact(agent_fd, &mut len_buf) {
        return None;
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if frame_type[0] != FRAME_INPUT || len < 8 {
        die("bad input frame");
    }
    let mut frame_request_id = [0u8; 8];
    if !try_read_exact(agent_fd, &mut frame_request_id) {
        return None;
    }
    let frame_request_id = u64::from_be_bytes(frame_request_id);
    let payload_len = len - 8;
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 && !try_read_exact(agent_fd, &mut payload) {
        return None;
    }
    if frame_request_id != request_id {
        return Some(InputFrame::Stale);
    }
    if payload_len > 0 {
        Some(InputFrame::Data(payload))
    } else {
        Some(InputFrame::Eof)
    }
}

fn drain_pty_input(agent_fd: c_int, pty_fd: c_int, request_id: u64) -> bool {
    match read_input_frame(agent_fd, request_id) {
        Some(InputFrame::Data(payload)) => {
            write_all(pty_fd, &payload);
            true
        }
        Some(InputFrame::Eof) => {
            write_all(pty_fd, &[0x04]);
            true
        }
        Some(InputFrame::Stale) => true,
        None => false,
    }
}

fn drain_pipe_input(agent_fd: c_int, stdin_fd: &mut c_int, request_id: u64) -> bool {
    match read_input_frame(agent_fd, request_id) {
        Some(InputFrame::Data(payload)) => {
            if *stdin_fd >= 0 && !write_all(*stdin_fd, &payload) {
                unsafe {
                    close(*stdin_fd);
                }
                *stdin_fd = -1;
            }
            true
        }
        Some(InputFrame::Eof) => {
            if *stdin_fd >= 0 {
                unsafe {
                    close(*stdin_fd);
                }
                *stdin_fd = -1;
            }
            true
        }
        Some(InputFrame::Stale) => true,
        None => false,
    }
}

fn handle_local_control(listener_fd: c_int, agent_fd: &mut c_int, request_id: u64) {
    let client_fd = unsafe { accept(listener_fd, ptr::null_mut(), ptr::null_mut()) };
    if client_fd < 0 {
        return;
    }
    let mut frame_type = [0u8; 1];
    let mut len = [0u8; 4];
    if !try_read_exact(client_fd, &mut frame_type) || !try_read_exact(client_fd, &mut len) {
        unsafe {
            close(client_fd);
        }
        return;
    }
    let len = u32::from_be_bytes(len);
    if frame_type[0] != FRAME_CONTROL_SNAPSHOT_EXIT || len != 0 {
        unsafe {
            close(client_fd);
        }
        return;
    }

    unsafe {
        sync();
        close(*agent_fd);
    }
    *agent_fd = -1;
    let snapshot_fd = connect_vsock(CONTROL_PORT);
    let _ = write_frame(snapshot_fd, FRAME_CONTROL_SNAPSHOT_EXIT, &request_id.to_be_bytes());
    unsafe {
        close(snapshot_fd);
    }
    reconnect_agent_fd(agent_fd);
    let _ = write_frame(client_fd, FRAME_CONTROL_OK, &[]);
    unsafe {
        close(client_fd);
    }
}

fn accept_channel_control(listener_fd: c_int) -> Option<c_int> {
    let client_fd = unsafe { accept(listener_fd, ptr::null_mut(), ptr::null_mut()) };
    if client_fd < 0 {
        return None;
    }
    let mut frame_type = [0u8; 1];
    let mut len = [0u8; 4];
    let ok = try_read_exact(client_fd, &mut frame_type)
        && try_read_exact(client_fd, &mut len)
        && frame_type[0] == FRAME_CONTROL_SNAPSHOT_EXIT
        && u32::from_be_bytes(len) == 0;
    if ok {
        unsafe {
            sync();
        }
        Some(client_fd)
    } else {
        unsafe {
            close(client_fd);
        }
        None
    }
}

fn close_channel_control(fd: c_int) {
    unsafe {
        close(fd);
    }
    let _ = fs::remove_file(CONTROL_SOCKET);
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

fn run_command(fd: &mut c_int) -> (u64, c_int) {
    let mut request_id = [0u8; 8];
    read_exact(*fd, &mut request_id);
    let request_id = u64::from_be_bytes(request_id);

    let cwd_len = read_u32(*fd) as usize;
    if cwd_len > 1024 * 1024 {
        die("bad cwd len");
    }
    let mut cwd = vec![0u8; cwd_len + 1];
    read_exact(*fd, &mut cwd[..cwd_len]);
    if cwd_len > 0 {
        unsafe {
            chdir(cwd.as_ptr() as *const c_char);
        }
    }

    let flags = read_u8(*fd);
    if flags & !REQUEST_FLAG_PTY != 0 {
        die("bad request flags");
    }
    let use_pty = flags & REQUEST_FLAG_PTY != 0;
    let (term, colorterm, rows, cols) = if use_pty {
        (
            read_string(*fd, 1024, "bad TERM len"),
            read_string(*fd, 1024, "bad COLORTERM len"),
            read_u16(*fd).max(1),
            read_u16(*fd).max(1),
        )
    } else {
        (String::new(), String::new(), 1, 1)
    };

    let argc = read_u32(*fd);
    if argc == 0 || argc > 4096 {
        die("bad argc");
    }

    let mut storage = Vec::with_capacity(argc as usize);
    let mut argv = Vec::with_capacity(argc as usize + 1);
    for _ in 0..argc {
        let len = read_u32(*fd) as usize;
        if len > 1024 * 1024 {
            die("bad argv len");
        }
        let mut bytes = vec![0u8; len + 1];
        read_exact(*fd, &mut bytes[..len]);
        storage.push(bytes);
    }
    for bytes in &storage {
        argv.push(bytes.as_ptr() as *const c_char);
    }
    argv.push(ptr::null());

    let status = if use_pty {
        run_pty_command(fd, request_id, &argv, &term, &colorterm, rows, cols)
    } else {
        run_pipe_command(fd, request_id, &argv)
    };

    (request_id, exit_status(status))
}

fn run_pty_command(
    fd: &mut c_int,
    request_id: u64,
    argv: &[*const c_char],
    term: &str,
    colorterm: &str,
    rows: u16,
    cols: u16,
) -> c_int {
    let mut pty_master = -1;
    let mut pty_slave = -1;
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
        die("openpty");
    }
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
        die("fork");
    }
    if pid == 0 {
        unsafe {
            close(pty_master);
            close(*fd);
            setsid();
            ioctl(pty_slave, TIOCSCTTY, 0);
            dup2(pty_slave, STDIN_FILENO);
            dup2(pty_slave, STDOUT_FILENO);
            dup2(pty_slave, STDERR_FILENO);
            if pty_slave > STDERR_FILENO {
                close(pty_slave);
            }
            if let Ok(name) = CString::new("TERM") {
                if let Ok(value) = CString::new(term) {
                    setenv(name.as_ptr(), value.as_ptr(), 1);
                }
            }
            if !colorterm.is_empty() {
                if let Ok(name) = CString::new("COLORTERM") {
                    if let Ok(value) = CString::new(colorterm) {
                        setenv(name.as_ptr(), value.as_ptr(), 1);
                    }
                }
            }
            set_default_exec_environment();
            set_lnx_request_id(request_id);
            execvp(argv[0], argv.as_ptr());
            exec_failed(argv[0]);
        }
    }
    unsafe {
        close(pty_slave);
    }
    set_nonblocking(pty_master);
    set_nonblocking(*fd);
    let control_fd = listen_unix(CONTROL_SOCKET);

    let mut buf = [0u8; 8192];
    let mut status = 0;
    let mut child_exited = false;
    let mut pty_eof = false;
    loop {
        let mut pollfds = [
            PollFd {
                fd: pty_master,
                events: POLLIN,
                revents: 0,
            },
            PollFd {
                fd: *fd,
                events: POLLIN,
                revents: 0,
            },
            PollFd {
                fd: control_fd,
                events: POLLIN,
                revents: 0,
            },
        ];
        let n = unsafe { poll(pollfds.as_mut_ptr(), 3, 100) };
        if n < 0 && errno() == EINTR {
            continue;
        }
        if n > 0 && pollfds[0].revents & (POLLIN | POLLHUP | POLLERR) != 0 {
            pty_eof |= drain_output(pty_master, fd, FRAME_OUTPUT, request_id, &mut buf);
        }
        if n > 0 && pollfds[1].revents & (POLLHUP | POLLERR) != 0 {
            reconnect_agent_fd(fd);
        }
        if n > 0 && pollfds[1].revents & POLLIN != 0 {
            if !drain_pty_input(*fd, pty_master, request_id) {
                reconnect_agent_fd(fd);
            }
        }
        if n > 0 && pollfds[2].revents & POLLIN != 0 {
            handle_local_control(control_fd, fd, request_id);
        }
        if !child_exited {
            let waited = unsafe { waitpid(pid, &mut status, WNOHANG) };
            if waited == pid {
                child_exited = true;
            } else if waited < 0 {
                let err = errno();
                if err == ECHILD {
                    status = 0;
                    child_exited = true;
                } else {
                    status = 127 << 8;
                    child_exited = true;
                }
            }
        }
        if child_exited {
            pty_eof |= drain_output(pty_master, fd, FRAME_OUTPUT, request_id, &mut buf);
        }
        if child_exited && pty_eof {
            break;
        }
    }
    unsafe {
        close(pty_master);
        close(control_fd);
    }
    let _ = fs::remove_file(CONTROL_SOCKET);

    status
}

fn run_pipe_command(fd: &mut c_int, request_id: u64, argv: &[*const c_char]) -> c_int {
    let mut stdin_pipe = [-1; 2];
    let mut stdout_pipe = [-1; 2];
    let mut stderr_pipe = [-1; 2];
    if unsafe { pipe(stdin_pipe.as_mut_ptr()) } < 0 {
        die("pipe stdin");
    }
    if unsafe { pipe(stdout_pipe.as_mut_ptr()) } < 0 {
        die("pipe stdout");
    }
    if unsafe { pipe(stderr_pipe.as_mut_ptr()) } < 0 {
        die("pipe stderr");
    }

    let pid = unsafe { fork() };
    if pid < 0 {
        die("fork");
    }
    if pid == 0 {
        unsafe {
            close(stdin_pipe[1]);
            close(stdout_pipe[0]);
            close(stderr_pipe[0]);
            close(*fd);
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
            set_default_exec_environment();
            set_lnx_request_id(request_id);
            execvp(argv[0], argv.as_ptr());
            exec_failed(argv[0]);
        }
    }

    unsafe {
        close(stdin_pipe[0]);
        close(stdout_pipe[1]);
        close(stderr_pipe[1]);
    }
    let mut stdin_write = stdin_pipe[1];
    let stdout_read = stdout_pipe[0];
    let stderr_read = stderr_pipe[0];
    set_nonblocking(stdout_read);
    set_nonblocking(stderr_read);
    set_nonblocking(*fd);
    let control_fd = listen_unix(CONTROL_SOCKET);

    let mut buf = [0u8; 8192];
    let mut status = 0;
    let mut child_exited = false;
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    loop {
        let mut pollfds = [
            PollFd {
                fd: stdout_read,
                events: POLLIN,
                revents: 0,
            },
            PollFd {
                fd: stderr_read,
                events: POLLIN,
                revents: 0,
            },
            PollFd {
                fd: *fd,
                events: POLLIN,
                revents: 0,
            },
            PollFd {
                fd: control_fd,
                events: POLLIN,
                revents: 0,
            },
        ];
        let n = unsafe { poll(pollfds.as_mut_ptr(), 4, 100) };
        if n < 0 && errno() == EINTR {
            continue;
        }
        if n > 0 && pollfds[0].revents & (POLLIN | POLLHUP | POLLERR) != 0 {
            stdout_eof |= drain_output(stdout_read, fd, FRAME_OUTPUT, request_id, &mut buf);
        }
        if n > 0 && pollfds[1].revents & (POLLIN | POLLHUP | POLLERR) != 0 {
            stderr_eof |= drain_output(stderr_read, fd, FRAME_STDERR, request_id, &mut buf);
        }
        if n > 0 && pollfds[2].revents & (POLLHUP | POLLERR) != 0 {
            reconnect_agent_fd(fd);
        }
        if n > 0 && pollfds[2].revents & POLLIN != 0 {
            if !drain_pipe_input(*fd, &mut stdin_write, request_id) {
                reconnect_agent_fd(fd);
            }
        }
        if n > 0 && pollfds[3].revents & POLLIN != 0 {
            handle_local_control(control_fd, fd, request_id);
        }
        if !child_exited {
            let waited = unsafe { waitpid(pid, &mut status, WNOHANG) };
            if waited == pid {
                child_exited = true;
            } else if waited < 0 {
                let err = errno();
                if err == ECHILD {
                    status = 0;
                    child_exited = true;
                } else {
                    status = 127 << 8;
                    child_exited = true;
                }
            }
        }
        if child_exited {
            if stdin_write >= 0 {
                unsafe {
                    close(stdin_write);
                }
                stdin_write = -1;
            }
            stdout_eof |= drain_output(stdout_read, fd, FRAME_OUTPUT, request_id, &mut buf);
            stderr_eof |= drain_output(stderr_read, fd, FRAME_STDERR, request_id, &mut buf);
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
        close(control_fd);
    }
    let _ = fs::remove_file(CONTROL_SOCKET);

    status
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
}

fn exec_failed(arg0: *const c_char) -> ! {
    unsafe {
        let msg = CStr::from_ptr(arg0).to_bytes();
        write_all(STDERR_FILENO, b"exec failed: ");
        write_all(STDERR_FILENO, msg);
        write_all(STDERR_FILENO, b"\n");
        _exit(127);
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

fn send_status(agent_fd: &Arc<Mutex<c_int>>, channel_id: u64, status: c_int) {
    let _ = write_message_locked(
        agent_fd,
        &Message::ExitStatus {
            channel_id,
            status: exit_status(status),
        },
    );
    let _ = write_message_locked(agent_fd, &Message::Close { channel_id });
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
    term: String,
    colorterm: String,
    rows: u16,
    cols: u16,
    rx: mpsc::Receiver<ChannelInput>,
) {
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
        let _ = write_message_locked(&agent_fd, &Message::Error { channel_id, message: "openpty failed".to_string() });
        send_status(&agent_fd, channel_id, 127 << 8);
        return;
    }
    let winsize = Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    unsafe {
        ioctl(pty_master, TIOCSWINSZ, &winsize as *const Winsize);
    }

    let pid = unsafe { fork() };
    if pid < 0 {
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
                unsafe { chdir(cwd.as_ptr()); }
            }
        }
        if let (Ok(name), Ok(value)) = (CString::new("TERM"), CString::new(term)) {
            unsafe { setenv(name.as_ptr(), value.as_ptr(), 1); }
        }
        if !colorterm.is_empty() {
            if let (Ok(name), Ok(value)) = (CString::new("COLORTERM"), CString::new(colorterm)) {
                unsafe { setenv(name.as_ptr(), value.as_ptr(), 1); }
            }
        }
        set_default_exec_environment();
        set_lnx_request_id(channel_id);
        set_lnx_control_socket(&control_socket);
        let (_storage, ptrs) = make_argv(&argv);
        unsafe {
            execvp(ptrs[0], ptrs.as_ptr());
            exec_failed(ptrs[0]);
        }
    }
    unsafe { close(pty_slave); }
    set_nonblocking(pty_master);
    let control_fd = listen_unix(&control_socket);
    let mut pending_control_fd = -1;
    let mut buf = [0u8; 8192];
    let mut status = 0;
    let mut child_exited = false;
    let mut pty_eof = false;
    loop {
        let mut pollfds = [
            PollFd { fd: pty_master, events: POLLIN, revents: 0 },
            PollFd { fd: control_fd, events: POLLIN, revents: 0 },
        ];
        let n = unsafe { poll(pollfds.as_mut_ptr(), 2, 25) };
        if n > 0 && pollfds[0].revents & (POLLIN | POLLHUP | POLLERR) != 0 {
            pty_eof |= drain_output_message(pty_master, &agent_fd, false, channel_id, &mut buf);
        }
        if n > 0 && pollfds[1].revents & POLLIN != 0 && pending_control_fd < 0 {
            if let Some(fd) = accept_channel_control(control_fd) {
                pending_control_fd = fd;
                let _ = write_message_locked(&agent_fd, &Message::SnapshotExit { channel_id });
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
                    let winsize = Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
                    unsafe { ioctl(pty_master, TIOCSWINSZ, &winsize as *const Winsize); }
                }
                ChannelInput::Close => {
                    unsafe { kill(pid, SIGTERM); }
                    status = 130 << 8;
                    break;
                }
                ChannelInput::SnapshotComplete => {
                    if pending_control_fd >= 0 {
                        let _ = write_frame(pending_control_fd, FRAME_CONTROL_OK, &[]);
                        unsafe { close(pending_control_fd); }
                        pending_control_fd = -1;
                    }
                }
                ChannelInput::SnapshotFailed => {
                    if pending_control_fd >= 0 {
                        unsafe { close(pending_control_fd); }
                        pending_control_fd = -1;
                    }
                }
            }
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
        if child_exited {
            pty_eof |= drain_output_message(pty_master, &agent_fd, false, channel_id, &mut buf);
        }
        if child_exited && pty_eof {
            break;
        }
    }
    unsafe { close(pty_master); }
    if pending_control_fd >= 0 {
        unsafe { close(pending_control_fd); }
    }
    close_channel_control_socket(control_fd, &control_socket);
    send_status(&agent_fd, channel_id, status);
}

fn run_channel_pipe(
    agent_fd: Arc<Mutex<c_int>>,
    channel_id: u64,
    argv: Vec<String>,
    cwd: String,
    rx: mpsc::Receiver<ChannelInput>,
) {
    let control_socket = channel_control_socket(channel_id);
    let mut stdin_pipe = [-1; 2];
    let mut stdout_pipe = [-1; 2];
    let mut stderr_pipe = [-1; 2];
    if unsafe { pipe(stdin_pipe.as_mut_ptr()) } < 0
        || unsafe { pipe(stdout_pipe.as_mut_ptr()) } < 0
        || unsafe { pipe(stderr_pipe.as_mut_ptr()) } < 0
    {
        send_status(&agent_fd, channel_id, 127 << 8);
        return;
    }
    let pid = unsafe { fork() };
    if pid < 0 {
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
            if stdin_pipe[0] > STDERR_FILENO { close(stdin_pipe[0]); }
            if stdout_pipe[1] > STDERR_FILENO { close(stdout_pipe[1]); }
            if stderr_pipe[1] > STDERR_FILENO { close(stderr_pipe[1]); }
        }
        if !cwd.is_empty() {
            if let Ok(cwd) = CString::new(cwd) {
                unsafe { chdir(cwd.as_ptr()); }
            }
        }
        set_default_exec_environment();
        set_lnx_request_id(channel_id);
        set_lnx_control_socket(&control_socket);
        let (_storage, ptrs) = make_argv(&argv);
        unsafe {
            execvp(ptrs[0], ptrs.as_ptr());
            exec_failed(ptrs[0]);
        }
    }
    unsafe {
        close(stdin_pipe[0]);
        close(stdout_pipe[1]);
        close(stderr_pipe[1]);
    }
    let mut stdin_write = stdin_pipe[1];
    let stdout_read = stdout_pipe[0];
    let stderr_read = stderr_pipe[0];
    set_nonblocking(stdout_read);
    set_nonblocking(stderr_read);
    let control_fd = listen_unix(&control_socket);
    let mut pending_control_fd = -1;
    let mut buf = [0u8; 8192];
    let mut status = 0;
    let mut child_exited = false;
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    loop {
        let mut pollfds = [
            PollFd { fd: stdout_read, events: POLLIN, revents: 0 },
            PollFd { fd: stderr_read, events: POLLIN, revents: 0 },
            PollFd { fd: control_fd, events: POLLIN, revents: 0 },
        ];
        let n = unsafe { poll(pollfds.as_mut_ptr(), 3, 25) };
        if n > 0 && pollfds[0].revents & (POLLIN | POLLHUP | POLLERR) != 0 {
            stdout_eof |= drain_output_message(stdout_read, &agent_fd, false, channel_id, &mut buf);
        }
        if n > 0 && pollfds[1].revents & (POLLIN | POLLHUP | POLLERR) != 0 {
            stderr_eof |= drain_output_message(stderr_read, &agent_fd, true, channel_id, &mut buf);
        }
        if n > 0 && pollfds[2].revents & POLLIN != 0 && pending_control_fd < 0 {
            if let Some(fd) = accept_channel_control(control_fd) {
                pending_control_fd = fd;
                let _ = write_message_locked(&agent_fd, &Message::SnapshotExit { channel_id });
            }
        }
        while let Ok(input) = rx.try_recv() {
            match input {
                ChannelInput::Data(bytes) if stdin_write >= 0 => {
                    if !write_all(stdin_write, &bytes) {
                        unsafe { close(stdin_write); }
                        stdin_write = -1;
                    }
                }
                ChannelInput::Eof if stdin_write >= 0 => {
                    unsafe { close(stdin_write); }
                    stdin_write = -1;
                }
                ChannelInput::Close => {
                    unsafe { kill(pid, SIGTERM); }
                    status = 130 << 8;
                    break;
                }
                ChannelInput::SnapshotComplete => {
                    if pending_control_fd >= 0 {
                        let _ = write_frame(pending_control_fd, FRAME_CONTROL_OK, &[]);
                        unsafe { close(pending_control_fd); }
                        pending_control_fd = -1;
                    }
                }
                ChannelInput::SnapshotFailed => {
                    if pending_control_fd >= 0 {
                        unsafe { close(pending_control_fd); }
                        pending_control_fd = -1;
                    }
                }
                _ => {}
            }
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
        if child_exited {
            if stdin_write >= 0 {
                unsafe { close(stdin_write); }
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
        if stdin_write >= 0 { close(stdin_write); }
        close(stdout_read);
        close(stderr_read);
    }
    if pending_control_fd >= 0 {
        unsafe { close(pending_control_fd); }
    }
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
            | Ok(ChannelInput::SnapshotFailed) => {}
            Ok(ChannelInput::Close) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = writer.shutdown(Shutdown::Both);
    let _ = write_message_locked(&agent_fd, &Message::Close { channel_id });
}

fn agent_loop() {
    let mut fd = connect_vsock(AGENT_PORT);
    let agent_fd = Arc::new(Mutex::new(fd));
    let _ = write_message_locked(&agent_fd, &Message::Hello { version: PROTOCOL_VERSION });
    let mut channels: Vec<(u64, mpsc::Sender<ChannelInput>)> = Vec::new();
    loop {
        let message = read_message(fd);
        let Some(message) = message else {
            unsafe {
                close(fd);
            }
            fd = reconnect_after_snapshot_point();
            if let Ok(mut shared) = agent_fd.lock() {
                *shared = fd;
            }
            let _ = write_message_locked(&agent_fd, &Message::Hello { version: PROTOCOL_VERSION });
            continue;
        };
        match message {
            Message::Hello { version } if version == PROTOCOL_VERSION => {}
            Message::OpenExec { channel_id, argv, cwd, pty, term, colorterm, rows, cols } => {
                let (tx, rx) = mpsc::channel();
                channels.push((channel_id, tx));
                let writer = Arc::clone(&agent_fd);
                if pty {
                    thread::spawn(move || run_channel_pty(writer, channel_id, argv, cwd, term, colorterm, rows, cols, rx));
                } else {
                    thread::spawn(move || run_channel_pipe(writer, channel_id, argv, cwd, rx));
                }
            }
            Message::OpenTcp { channel_id, host, port } => {
                let (tx, rx) = mpsc::channel();
                channels.push((channel_id, tx));
                let writer = Arc::clone(&agent_fd);
                thread::spawn(move || run_channel_tcp(writer, channel_id, host, port, rx));
            }
            Message::Data { channel_id, bytes } => {
                if let Some((_, tx)) = channels.iter().find(|(id, _)| *id == channel_id) {
                    let _ = tx.send(ChannelInput::Data(bytes));
                }
            }
            Message::Eof { channel_id } => {
                if let Some((_, tx)) = channels.iter().find(|(id, _)| *id == channel_id) {
                    let _ = tx.send(ChannelInput::Eof);
                }
            }
            Message::WindowResize { channel_id, rows, cols } => {
                if let Some((_, tx)) = channels.iter().find(|(id, _)| *id == channel_id) {
                    let _ = tx.send(ChannelInput::Resize(rows, cols));
                }
            }
            Message::Close { channel_id } => {
                if let Some((_, tx)) = channels.iter().find(|(id, _)| *id == channel_id) {
                    let _ = tx.send(ChannelInput::Close);
                }
                channels.retain(|(id, _)| *id != channel_id);
            }
            Message::CheckpointCreated { channel_id } => {
                if let Some((_, tx)) = channels.iter().find(|(id, _)| *id == channel_id) {
                    let _ = tx.send(ChannelInput::SnapshotComplete);
                }
            }
            Message::Error { channel_id, .. } => {
                if let Some((_, tx)) = channels.iter().find(|(id, _)| *id == channel_id) {
                    let _ = tx.send(ChannelInput::SnapshotFailed);
                }
            }
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
                let _ = write_message_locked(&agent_fd, &Message::Hello { version: PROTOCOL_VERSION });
            }
            _ => {}
        }
    }
}

pub fn main() {
    let args = env::args().collect::<Vec<_>>();
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
