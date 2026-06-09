use std::env;
use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void, CStr, CString};
use std::io::Error;
use std::mem::size_of;
use std::ptr;

const AF_VSOCK: c_int = 40;
const SOCK_STREAM: c_int = 1;
const VMADDR_CID_ANY: u32 = 0xffff_ffff;
const EINTR: c_int = 4;
const EAGAIN: c_int = 11;
const EIO: c_int = 5;
const O_CLOEXEC: c_int = 0o2000000;
const O_NONBLOCK: c_int = 0o4000;
const F_GETFD: c_int = 1;
const F_SETFD: c_int = 2;
const F_GETFL: c_int = 3;
const F_SETFL: c_int = 4;
const FD_CLOEXEC: c_int = 1;
const POLLIN: i16 = 0x0001;
const POLLERR: i16 = 0x0008;
const POLLHUP: i16 = 0x0010;
const SIGPIPE: c_int = 13;
const SIGWINCH: c_int = 28;
const SIG_IGN: usize = 1;
const STDIN_FILENO: c_int = 0;
const STDOUT_FILENO: c_int = 1;
const STDERR_FILENO: c_int = 2;
const TIOCSCTTY: c_ulong = 0x540e;
const TIOCSWINSZ: c_ulong = 0x5414;
const WNOHANG: c_int = 1;
const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024;
const FRAME_OUTPUT: u8 = b'O';
const FRAME_STATUS: u8 = b'S';
const FRAME_INPUT: u8 = b'I';
const FRAME_RESIZE: u8 = b'W';

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
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
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
    fn calloc(nmemb: usize, size: usize) -> *mut c_void;
    fn close(fd: c_int) -> c_int;
    fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
    fn execvp(file: *const c_char, argv: *const *const c_char) -> c_int;
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    fn fork() -> c_int;
    fn free(ptr: *mut c_void);
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn kill(pid: c_int, sig: c_int) -> c_int;
    fn listen(fd: c_int, backlog: c_int) -> c_int;
    fn malloc(size: usize) -> *mut c_void;
    fn mkdir(path: *const c_char, mode: c_uint) -> c_int;
    fn mount(
        source: *const c_char,
        target: *const c_char,
        filesystemtype: *const c_char,
        mountflags: c_ulong,
        data: *const c_void,
    ) -> c_int;
    fn openpty(
        amaster: *mut c_int,
        aslave: *mut c_int,
        name: *mut c_char,
        termp: *const c_void,
        winp: *const Winsize,
    ) -> c_int;
    fn poll(fds: *mut PollFd, nfds: c_ulong, timeout: c_int) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    fn signal(signum: c_int, handler: usize) -> usize;
    fn socket(domain: c_int, typ: c_int, protocol: c_int) -> c_int;
    fn setsid() -> c_int;
    fn strtoul(nptr: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_ulong;
    fn usleep(usec: c_uint) -> c_int;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    fn _exit(status: c_int) -> !;
}

fn errno() -> c_int {
    Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn die(msg: &str) -> ! {
    eprintln!("{msg}: {}", Error::last_os_error());
    unsafe { _exit(125) }
}

fn mkdir_p(path: &CStr) {
    let bytes = path.to_bytes();
    let mut current = Vec::with_capacity(bytes.len() + 1);
    for &byte in bytes {
        current.push(byte);
        if byte == b'/' && current.len() > 1 {
            current.push(0);
            unsafe {
                mkdir(current.as_ptr() as *const c_char, 0o755);
            }
            current.pop();
        }
    }
    current.push(0);
    unsafe {
        mkdir(current.as_ptr() as *const c_char, 0o755);
    }
}

fn listen_vsock(port: u32) -> c_int {
    let fd = unsafe { socket(AF_VSOCK, SOCK_STREAM, 0) };
    if fd < 0 {
        die("socket(AF_VSOCK)");
    }

    let addr = SockaddrVm {
        svm_family: AF_VSOCK as u16,
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: VMADDR_CID_ANY,
        svm_flags: 0,
        svm_zero: [0; 3],
    };
    let ret = unsafe {
        bind(
            fd,
            &addr as *const SockaddrVm as *const Sockaddr,
            size_of::<SockaddrVm>() as c_uint,
        )
    };
    if ret < 0 {
        die("bind(vsock)");
    }
    if unsafe { listen(fd, 32) } < 0 {
        die("listen(vsock)");
    }
    fd
}

fn write_all(fd: c_int, mut buf: &[u8]) {
    while !buf.is_empty() {
        let n = unsafe { write(fd, buf.as_ptr() as *const c_void, buf.len()) };
        if n < 0 {
            if errno() == EINTR {
                continue;
            }
            return;
        }
        buf = &buf[n as usize..];
    }
}

fn write_u32_be(fd: c_int, value: u32) {
    write_all(fd, &value.to_be_bytes());
}

fn write_frame(fd: c_int, frame_type: u8, payload: &[u8]) {
    write_all(fd, &[frame_type]);
    write_u32_be(fd, payload.len() as u32);
    write_all(fd, payload);
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

fn read_u32(fd: c_int) -> u32 {
    let mut buf = [0u8; 4];
    read_exact(fd, &mut buf);
    u32::from_be_bytes(buf)
}

struct CommandRequest {
    argv: *mut *mut c_char,
    rows: u16,
    cols: u16,
}

fn read_command_request(fd: c_int) -> CommandRequest {
    let argc = read_u32(fd);
    if argc == 0 || argc > 4096 {
        die("bad argc");
    }

    let argv =
        unsafe { calloc(argc as usize + 1, size_of::<*mut c_char>()) as *mut *mut c_char };
    if argv.is_null() {
        die("calloc");
    }

    for i in 0..argc {
        let len = read_u32(fd);
        if len > 1024 * 1024 {
            die("bad argv len");
        }
        let ptr = unsafe { malloc(len as usize + 1) as *mut c_char };
        if ptr.is_null() {
            die("malloc argv");
        }
        let bytes = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, len as usize) };
        read_exact(fd, bytes);
        unsafe {
            *ptr.add(len as usize) = 0;
            *argv.add(i as usize) = ptr;
        }
    }

    let rows = clamp_u16(read_u32(fd));
    let cols = clamp_u16(read_u32(fd));

    CommandRequest { argv, rows, cols }
}

fn clamp_u16(value: u32) -> u16 {
    value.min(u16::MAX as u32) as u16
}

fn free_argv(argv: *mut *mut c_char) {
    if argv.is_null() {
        return;
    }
    let mut p = argv;
    unsafe {
        while !(*p).is_null() {
            free(*p as *mut c_void);
            p = p.add(1);
        }
        free(argv as *mut c_void);
    }
}

fn set_cloexec(fd: c_int) {
    let flags = unsafe { fcntl(fd, F_GETFD) };
    if flags >= 0 {
        unsafe {
            fcntl(fd, F_SETFD, flags | FD_CLOEXEC);
        }
    }
}

fn set_nonblocking(fd: c_int) {
    let flags = unsafe { fcntl(fd, F_GETFL) };
    if flags >= 0 {
        unsafe {
            fcntl(fd, F_SETFL, flags | O_NONBLOCK);
        }
    }
}

fn winsize(rows: u16, cols: u16) -> Winsize {
    Winsize {
        ws_row: if rows == 0 { 24 } else { rows },
        ws_col: if cols == 0 { 80 } else { cols },
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

fn set_pty_size(master: c_int, rows: u16, cols: u16, child_pid: c_int) {
    let ws = winsize(rows, cols);
    unsafe {
        ioctl(master, TIOCSWINSZ, &ws);
        if child_pid > 0 {
            kill(-child_pid, SIGWINCH);
        }
    }
}

fn write_all_retry(fd: c_int, mut buf: &[u8]) {
    while !buf.is_empty() {
        let n = unsafe { write(fd, buf.as_ptr() as *const c_void, buf.len()) };
        if n < 0 {
            match errno() {
                EINTR => continue,
                EAGAIN => {
                    unsafe {
                        usleep(10000);
                    }
                    continue;
                }
                _ => return,
            }
        }
        buf = &buf[n as usize..];
    }
}

fn run_command(request: &CommandRequest, out_fd: c_int) -> c_int {
    set_cloexec(out_fd);

    let mut master = -1;
    let mut slave = -1;
    let ws = winsize(request.rows, request.cols);
    if unsafe {
        openpty(
            &mut master,
            &mut slave,
            ptr::null_mut(),
            ptr::null(),
            &ws,
        )
    } < 0
    {
        die("openpty");
    }

    let pid = unsafe { fork() };
    if pid < 0 {
        die("fork");
    }
    if pid == 0 {
        unsafe {
            close(master);
            setsid();
            ioctl(slave, TIOCSCTTY, 0);
            dup2(slave, STDIN_FILENO);
            dup2(slave, STDOUT_FILENO);
            dup2(slave, STDERR_FILENO);
            if slave > STDERR_FILENO {
                close(slave);
            }
            execvp(*request.argv, request.argv as *const *const c_char);
            write_exec_error(*request.argv);
            _exit(127);
        }
    }

    unsafe {
        close(slave);
    }
    set_nonblocking(master);

    let mut buf = [0u8; 8192];
    let mut status = 0;
    let mut child_exited = false;
    loop {
        let mut fds = [
            PollFd {
                fd: master,
                events: POLLIN,
                revents: 0,
            },
            PollFd {
                fd: out_fd,
                events: POLLIN,
                revents: 0,
            },
        ];
        let poll_ret = unsafe { poll(fds.as_mut_ptr(), fds.len() as c_ulong, 100) };
        if poll_ret < 0 && errno() == EINTR {
            continue;
        }
        if poll_ret > 0 {
            if fds[0].revents & (POLLIN | POLLHUP | POLLERR) != 0 {
                drain_pty_output(master, out_fd, &mut buf);
            }
            if fds[1].revents & (POLLIN | POLLHUP | POLLERR) != 0
                && !read_host_frame(out_fd, master, pid, &mut buf)
            {
                break;
            }
        }

        if !child_exited {
            let waited = unsafe { waitpid(pid, &mut status, WNOHANG) };
            if waited == pid {
                child_exited = true;
            }
        }
        if child_exited {
            drain_pty_output(master, out_fd, &mut buf);
            break;
        }
    }

    unsafe {
        close(master);
    }
    if !child_exited {
        unsafe {
            waitpid(pid, &mut status, 0);
        }
    }

    if (status & 0x7f) == 0 {
        (status >> 8) & 0xff
    } else {
        128 + (status & 0x7f)
    }
}

fn drain_pty_output(master: c_int, out_fd: c_int, buf: &mut [u8]) {
    loop {
        let n = unsafe { read(master, buf.as_mut_ptr() as *mut c_void, buf.len()) };
        if n > 0 {
            write_frame(out_fd, FRAME_OUTPUT, &buf[..n as usize]);
            continue;
        }
        if n < 0 {
            match errno() {
                EINTR => continue,
                EAGAIN | EIO => break,
                _ => break,
            }
        }
        break;
    }
}

fn read_host_frame(fd: c_int, pty_master: c_int, child_pid: c_int, buf: &mut [u8]) -> bool {
    let mut frame_type = [0u8; 1];
    let n = unsafe { read(fd, frame_type.as_mut_ptr() as *mut c_void, 1) };
    if n == 0 {
        return false;
    }
    if n < 0 {
        if errno() == EINTR || errno() == EAGAIN {
            return true;
        }
        return false;
    }

    let len = read_u32(fd);
    if len > MAX_FRAME_SIZE {
        die("host frame too large");
    }

    match frame_type[0] {
        FRAME_INPUT => {
            let mut remaining = len as usize;
            while remaining > 0 {
                let chunk = remaining.min(buf.len());
                read_exact(fd, &mut buf[..chunk]);
                write_all_retry(pty_master, &buf[..chunk]);
                remaining -= chunk;
            }
        }
        FRAME_RESIZE => {
            if len != 8 {
                die("bad resize frame");
            }
            let rows = clamp_u16(read_u32(fd));
            let cols = clamp_u16(read_u32(fd));
            set_pty_size(pty_master, rows, cols, child_pid);
        }
        _ => {
            let mut remaining = len as usize;
            while remaining > 0 {
                let chunk = remaining.min(buf.len());
                read_exact(fd, &mut buf[..chunk]);
                remaining -= chunk;
            }
        }
    }
    true
}

fn write_exec_error(command: *const c_char) {
    if command.is_null() {
        write_all(STDERR_FILENO, b"command not found\n");
        return;
    }

    let command = unsafe { CStr::from_ptr(command) }.to_bytes();
    write_all(STDERR_FILENO, b"command not found: ");
    write_all(STDERR_FILENO, command);
    write_all(STDERR_FILENO, b"\n");
}

fn main() {
    let args = env::args().collect::<Vec<_>>();
    if args.len() != 5 {
        unsafe { _exit(125) }
    }

    let home_tag = CString::new(args[1].as_str()).unwrap();
    let host_home = CString::new(args[2].as_str()).unwrap();
    let port = unsafe { strtoul(CString::new(args[4].as_str()).unwrap().as_ptr(), ptr::null_mut(), 10) }
        as u32;

    unsafe {
        signal(SIGPIPE, SIG_IGN);
    }
    mkdir_p(&host_home);

    let virtiofs = c"virtiofs";
    let mount_ret = unsafe {
        mount(
            home_tag.as_ptr(),
            host_home.as_ptr(),
            virtiofs.as_ptr(),
            0,
            ptr::null(),
        )
    };
    if mount_ret < 0 && errno() != 16 {
        die("mount home");
    }
    if std::env::set_current_dir(args[3].as_str()).is_err() {
        die("chdir");
    }

    let listener = listen_vsock(port);
    loop {
        let fd = unsafe { accept(listener, ptr::null_mut(), ptr::null_mut()) };
        if fd < 0 {
            if errno() == EINTR {
                continue;
            }
            die("accept(vsock)");
        }
        let request = read_command_request(fd);
        let status = run_command(&request, fd);
        free_argv(request.argv);
        write_frame(fd, FRAME_STATUS, &status.to_be_bytes());
        unsafe {
            close(fd);
        }
    }
}
