use std::env;
use std::ffi::CString;
use std::fs;
use std::io::{ErrorKind, IsTerminal, Read, Write};
use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

const UBUNTU_IMAGE: &str = "ubuntu:26.04";
const ROOTFS_NAME: &str = "ubuntu-26.04";
const HOME_TAG: &str = "krun-home";
const RUNNER_PORT: u32 = 10240;
const SNAPSHOT_DIR_NAME: &str = "krun-snapshot-v19";
const SNAPSHOT_METADATA: &str = "ubuntu-snapshot-v19-interactive-pty\n";
const RUNNER_PATH: &str = "/usr/local/bin/krun-command-runner";
const VSOCK_SOCKET: &str = "run/krun-command-runner.sock";
const DISABLE_SNAPSHOT_RESTORE_ENV: &str = "KRUN_DISABLE_SNAPSHOT_RESTORE";
const ENOENT: i32 = 2;
const CTRL_D: u8 = 4;
const FRAME_OUTPUT: u8 = b'O';
const FRAME_STATUS: u8 = b'S';
const FRAME_INPUT: u8 = b'I';
const FRAME_RESIZE: u8 = b'W';
const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024;

fn main() -> Result<()> {
    let command = parse_command_args()?;
    let terminal_state = terminal_state();
    let home = dirs::home_dir().context("could not resolve home directory")?;
    let cwd = env::current_dir().context("current directory")?;
    if !cwd.starts_with(&home) {
        bail!(
            "current directory {} is not inside home {}; this example mounts only home",
            cwd.display(),
            home.display()
        );
    }

    let state_dir = home.join(".libkrun");
    let run_dir = state_dir.join("run");
    let rootfs = state_dir.join("rootfs").join(ROOTFS_NAME);
    let snapshot = state_dir.join(SNAPSHOT_DIR_NAME);
    let socket = rootfs.join(VSOCK_SOCKET);
    let console_output = run_dir.join("krun.console.log");
    let restore_disabled = env::var_os(DISABLE_SNAPSHOT_RESTORE_ENV).is_some();
    let have_snapshot = !restore_disabled && usable_snapshot(&snapshot);
    if !have_snapshot && snapshot.exists() {
        fs::remove_dir_all(&snapshot)
            .with_context(|| format!("remove incompatible snapshot {}", snapshot.display()))?;
    }

    fs::create_dir_all(&state_dir).with_context(|| format!("create {}", state_dir.display()))?;
    fs::create_dir_all(&run_dir).with_context(|| format!("create {}", run_dir.display()))?;
    install_firmware(&state_dir)?;
    ensure_rootfs(&rootfs)?;
    install_resolv_conf(&rootfs)?;
    install_guest_runner(&rootfs)?;
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let _ = fs::remove_file(&socket);

    let (run_tx, run_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let command_done = Arc::new(AtomicBool::new(false));

    unsafe {
        call_i32(
            libkrun::krun_set_log_level(2),
            "libkrun::krun_set_log_level",
        )?;
        let ctx = libkrun::krun_create_ctx();
        if ctx < 0 {
            bail_krun(ctx, "libkrun::krun_create_ctx")?;
        }
        let ctx = ctx as u32;

        let console_output_c = cstring_path(&console_output)?;
        call_i32(
            libkrun::krun_set_console_output(ctx, console_output_c.as_ptr()),
            "libkrun::krun_set_console_output",
        )?;

        call_i32(
            libkrun::krun_set_vm_config(ctx, 2, 8192),
            "libkrun::krun_set_vm_config",
        )?;

        let rootfs_c = cstring_path(&rootfs)?;
        call_i32(
            libkrun::krun_set_root(ctx, rootfs_c.as_ptr()),
            "libkrun::krun_set_root",
        )?;

        let home_tag_c = CString::new(HOME_TAG)?;
        let home_c = cstring_path(&home)?;
        call_i32(
            libkrun::krun_add_virtiofs3(ctx, home_tag_c.as_ptr(), home_c.as_ptr(), 0, false),
            "libkrun::krun_add_virtiofs3(home)",
        )?;

        let socket_c = cstring_path(&socket)?;
        call_i32(
            libkrun::krun_add_vsock_port2(ctx, RUNNER_PORT, socket_c.as_ptr(), true),
            "libkrun::krun_add_vsock_port2(runner)",
        )?;

        if have_snapshot {
            let snapshot_c = cstring_path(&snapshot)?;
            call_i32(
                libkrun::krun_set_snapshot_path(ctx, snapshot_c.as_ptr()),
                "libkrun::krun_set_snapshot_path",
            )?;
        }

        configure_runner(ctx, &home, &cwd)?;
        spawn_command_client(
            ctx,
            have_snapshot,
            socket.clone(),
            console_output.clone(),
            terminal_state.clone(),
            command,
            run_rx,
            done_tx,
        );
        spawn_snapshot_after_command(
            ctx,
            snapshot.clone(),
            terminal_state.clone(),
            command_done.clone(),
            done_rx,
        );
        run_tx.send(()).context("start command client")?;

        let rc = libkrun::krun_start_enter(ctx);
        if rc != 0 && have_snapshot {
            restart_without_snapshot(&snapshot, &terminal_state)?;
        }
        if !command_done.load(Ordering::SeqCst) {
            restore_terminal(&terminal_state);
            bail!("{}", vm_exit_detail(rc, &console_output));
        }
        bail_krun(rc, "libkrun::krun_start_enter")?;
    }

    Ok(())
}

fn restart_without_snapshot(snapshot: &Path, terminal_state: &Option<String>) -> Result<()> {
    let _ = fs::remove_dir_all(snapshot);
    restore_terminal(terminal_state);
    let status = Command::new(env::current_exe().context("current executable")?)
        .args(env::args_os().skip(1))
        .env(DISABLE_SNAPSHOT_RESTORE_ENV, "1")
        .status()
        .context("restart without snapshot restore")?;
    std::process::exit(status.code().unwrap_or(1));
}

fn parse_command_args() -> Result<Vec<String>> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        bail!("usage: krun <command> [args...]");
    }
    if args[0].is_empty() {
        bail!("command must not be empty");
    }
    Ok(args)
}

fn ensure_rootfs(rootfs: &Path) -> Result<()> {
    if !rootfs.exists() {
        fs::create_dir_all(rootfs).with_context(|| format!("create {}", rootfs.display()))?;
    }
    if !rootfs.join("bin/sh").exists() {
        fs::remove_dir_all(rootfs)
            .with_context(|| format!("remove incomplete rootfs {}", rootfs.display()))?;
        fs::create_dir_all(rootfs).with_context(|| format!("create {}", rootfs.display()))?;

        let status = Command::new("docker")
            .args(["image", "inspect", UBUNTU_IMAGE])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("docker image inspect")?;
        if !status.success() {
            run(Command::new("docker").args(["pull", UBUNTU_IMAGE]))?;
        }

        let output = Command::new("docker")
            .args(["create", UBUNTU_IMAGE])
            .output()
            .context("docker create")?;
        if !output.status.success() {
            bail!(
                "docker create failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let container_id = String::from_utf8(output.stdout)
            .context("docker create output")?
            .trim()
            .to_string();

        let export = Command::new("docker")
            .args(["export", &container_id])
            .stdout(Stdio::piped())
            .spawn()
            .context("docker export")?;
        let export_stdout = export.stdout.context("docker export stdout")?;
        let mut tar = Command::new("tar")
            .args(["-xpf", "-", "-C"])
            .arg(rootfs)
            .stdin(Stdio::from(export_stdout))
            .spawn()
            .context("tar extract rootfs")?;
        let tar_status = tar.wait().context("wait for tar")?;
        let _ = Command::new("docker")
            .args(["rm", &container_id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !tar_status.success() {
            bail!("tar extraction failed with {tar_status}");
        }
    }
    Ok(())
}

fn usable_snapshot(snapshot: &Path) -> bool {
    snapshot.join("vmstate.bin").exists()
        && snapshot.join("pages.img").exists()
        && fs::read_to_string(snapshot.join("metadata"))
            .map(|metadata| metadata == SNAPSHOT_METADATA)
            .unwrap_or(false)
}

fn install_firmware(state_dir: &Path) -> Result<()> {
    let lib_dir = state_dir.join("lib");
    fs::create_dir_all(&lib_dir).with_context(|| format!("create {}", lib_dir.display()))?;
    let firmware = lib_dir.join("libkrunfw.5.dylib");
    fs::write(
        &firmware,
        include_bytes!(concat!(env!("OUT_DIR"), "/libkrunfw.5.dylib")),
    )
    .with_context(|| format!("write {}", firmware.display()))?;
    // SAFETY: This process controls its own environment before libkrun lazily
    // dlopens libkrunfw.
    unsafe {
        env::set_var("KRUNFW_PATH", &firmware);
    }
    Ok(())
}

fn install_guest_runner(rootfs: &Path) -> Result<()> {
    let runner_path = rootfs.join(RUNNER_PATH.trim_start_matches('/'));
    if let Some(parent) = runner_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    fs::write(
        &runner_path,
        include_bytes!(concat!(env!("OUT_DIR"), "/krun-command-runner")),
    )
    .with_context(|| format!("write {}", runner_path.display()))?;
    fs::set_permissions(&runner_path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod 0755 {}", runner_path.display()))?;
    Ok(())
}

fn install_resolv_conf(rootfs: &Path) -> Result<()> {
    let resolv_conf = rootfs.join("etc/resolv.conf");
    if fs::symlink_metadata(&resolv_conf)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        fs::remove_file(&resolv_conf)
            .with_context(|| format!("remove symlink {}", resolv_conf.display()))?;
    }
    if let Some(parent) = resolv_conf.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    let mut contents = String::from("# Generated by krun from the host resolver configuration.\n");
    for nameserver in host_nameservers() {
        contents.push_str("nameserver ");
        contents.push_str(&nameserver);
        contents.push('\n');
    }
    contents.push_str("options timeout:2 attempts:2\n");

    fs::write(&resolv_conf, contents)
        .with_context(|| format!("write {}", resolv_conf.display()))?;
    fs::set_permissions(&resolv_conf, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("chmod 0644 {}", resolv_conf.display()))?;
    Ok(())
}

fn host_nameservers() -> Vec<String> {
    let mut nameservers = Vec::new();
    collect_scutil_nameservers(&mut nameservers);
    collect_resolv_conf_nameservers(&mut nameservers);
    if nameservers.is_empty() {
        nameservers.push("1.1.1.1".to_string());
        nameservers.push("2606:4700:4700::1111".to_string());
    }
    nameservers
}

fn collect_scutil_nameservers(nameservers: &mut Vec<String>) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let Ok(output) = Command::new("scutil").arg("--dns").output() else {
        return;
    };
    if !output.status.success() {
        return;
    }

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim_start().starts_with("nameserver[") {
            push_nameserver(nameservers, value.trim());
        }
    }
}

fn collect_resolv_conf_nameservers(nameservers: &mut Vec<String>) {
    let Ok(contents) = fs::read_to_string("/etc/resolv.conf") else {
        return;
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        if parts.next() == Some("nameserver") {
            if let Some(nameserver) = parts.next() {
                push_nameserver(nameservers, nameserver);
            }
        }
    }
}

fn push_nameserver(nameservers: &mut Vec<String>, nameserver: &str) {
    let nameserver = nameserver.trim_matches(['[', ']']);
    let Ok(ip) = nameserver.parse::<IpAddr>() else {
        return;
    };
    if ip.is_loopback() || ip.is_unspecified() {
        return;
    }
    if !nameservers.iter().any(|existing| existing == nameserver) {
        nameservers.push(nameserver.to_string());
    }
}

unsafe fn configure_runner(ctx: u32, home: &Path, cwd: &Path) -> Result<()> {
    let exec_path = CString::new(RUNNER_PATH)?;
    let home_tag_c = CString::new(HOME_TAG)?;
    let home_c = cstring_path(home)?;
    let cwd_c = cstring_path(cwd)?;
    let port_c = CString::new(RUNNER_PORT.to_string())?;
    let argv = [
        home_tag_c.as_ptr(),
        home_c.as_ptr(),
        cwd_c.as_ptr(),
        port_c.as_ptr(),
        ptr::null(),
    ];
    let envp = [
        c"PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".as_ptr(),
        c"TERM=xterm-256color".as_ptr(),
        c"LANG=C.UTF-8".as_ptr(),
        ptr::null(),
    ];
    call_i32(
        unsafe { libkrun::krun_set_exec(ctx, exec_path.as_ptr(), argv.as_ptr(), envp.as_ptr()) },
        "libkrun::krun_set_exec",
    )
}

fn spawn_command_client(
    ctx: u32,
    have_snapshot: bool,
    socket: PathBuf,
    console_output: PathBuf,
    terminal_state: Option<String>,
    command: Vec<String>,
    start: mpsc::Receiver<()>,
    done: mpsc::Sender<Result<CommandResult, String>>,
) {
    thread::spawn(move || {
        let stop_input = Arc::new(AtomicBool::new(false));
        let mut raw_terminal = false;
        let result = (|| -> Result<CommandResult> {
            start.recv().context("wait for VM start")?;
            if have_snapshot {
                arm_dirty_tracking(ctx)?;
            }
            let stdin_is_terminal = std::io::stdin().is_terminal();
            raw_terminal = stdin_is_terminal && terminal_state.is_some();
            if raw_terminal {
                set_terminal_raw().context("set host terminal raw mode")?;
            }

            let mut stream = connect_unix(&socket, Duration::from_secs(30)).with_context(|| {
                format!(
                    "connect {}{}",
                    socket.display(),
                    console_log_hint(&console_output)
                )
            })?;
            let size = terminal_size();
            write_command_request(&mut stream, &command, size).context("send command")?;

            let writer = Arc::new(Mutex::new(
                stream.try_clone().context("clone command stream")?,
            ));
            let _stdin_thread =
                spawn_stdin_forward(writer.clone(), stop_input.clone(), stdin_is_terminal);
            let _resize_thread = if stdin_is_terminal {
                Some(spawn_resize_forward(writer, stop_input.clone(), size))
            } else {
                None
            };

            read_runner_frames(&mut stream)
        })();
        stop_input.store(true, Ordering::SeqCst);
        if raw_terminal {
            restore_terminal(&terminal_state);
        }
        let _ = done.send(result.map_err(|e| format!("{e:#}")));
    });
}

struct CommandResult {
    exit_status: i32,
}

fn arm_dirty_tracking(ctx: u32) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let rc = libkrun::krun_arm_dirty_tracking(ctx);
        if rc == 0 {
            return Ok(());
        }
        if rc != -ENOENT || Instant::now() >= deadline {
            bail_krun(rc, "libkrun::krun_arm_dirty_tracking")?;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct TerminalSize {
    rows: u32,
    cols: u32,
}

fn write_command_request(
    stream: &mut UnixStream,
    command: &[String],
    size: TerminalSize,
) -> Result<()> {
    write_u32(
        stream,
        command
            .len()
            .try_into()
            .context("too many command arguments")?,
    )?;
    for arg in command {
        let bytes = arg.as_bytes();
        write_u32(
            stream,
            bytes
                .len()
                .try_into()
                .context("command argument too large")?,
        )?;
        stream.write_all(bytes)?;
    }
    write_u32(stream, size.rows)?;
    write_u32(stream, size.cols)?;
    Ok(())
}

fn write_u32(stream: &mut UnixStream, value: u32) -> Result<()> {
    stream.write_all(&value.to_be_bytes())?;
    Ok(())
}

fn read_u32(stream: &mut UnixStream) -> Result<u32> {
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf)?;
    Ok(u32::from_be_bytes(buf))
}

fn write_frame(stream: &mut UnixStream, frame_type: u8, payload: &[u8]) -> Result<()> {
    stream.write_all(&[frame_type])?;
    write_u32(stream, payload.len().try_into().context("frame too large")?)?;
    stream.write_all(payload)?;
    Ok(())
}

fn write_locked_frame(
    writer: &Arc<Mutex<UnixStream>>,
    frame_type: u8,
    payload: &[u8],
) -> Result<()> {
    let mut stream = writer
        .lock()
        .map_err(|_| anyhow::anyhow!("stream lock poisoned"))?;
    write_frame(&mut stream, frame_type, payload)
}

fn write_resize_frame(writer: &Arc<Mutex<UnixStream>>, size: TerminalSize) -> Result<()> {
    let mut payload = [0u8; 8];
    payload[..4].copy_from_slice(&size.rows.to_be_bytes());
    payload[4..].copy_from_slice(&size.cols.to_be_bytes());
    write_locked_frame(writer, FRAME_RESIZE, &payload)
}

fn spawn_stdin_forward(
    writer: Arc<Mutex<UnixStream>>,
    stop: Arc<AtomicBool>,
    stdin_is_terminal: bool,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 8192];
        while !stop.load(Ordering::SeqCst) {
            match stdin.read(&mut buf) {
                Ok(0) if stdin_is_terminal => continue,
                Ok(0) => {
                    let _ = write_locked_frame(&writer, FRAME_INPUT, &[CTRL_D]);
                    break;
                }
                Ok(n) => {
                    if write_locked_frame(&writer, FRAME_INPUT, &buf[..n]).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    })
}

fn spawn_resize_forward(
    writer: Arc<Mutex<UnixStream>>,
    stop: Arc<AtomicBool>,
    mut last_size: TerminalSize,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(250));
            let size = terminal_size();
            if size != last_size {
                if write_resize_frame(&writer, size).is_err() {
                    break;
                }
                last_size = size;
            }
        }
    })
}

fn read_runner_frames(stream: &mut UnixStream) -> Result<CommandResult> {
    loop {
        let mut frame_type = [0u8; 1];
        stream
            .read_exact(&mut frame_type)
            .context("runner closed before exit status")?;
        let len = read_u32(stream).context("read runner frame length")?;
        if len > MAX_FRAME_SIZE {
            bail!("runner frame too large: {len}");
        }

        match frame_type[0] {
            FRAME_OUTPUT => write_stdout_frame(stream, len)?,
            FRAME_STATUS => {
                if len != 4 {
                    bail!("bad runner status frame length: {len}");
                }
                let mut status = [0u8; 4];
                stream
                    .read_exact(&mut status)
                    .context("read runner exit status")?;
                return Ok(CommandResult {
                    exit_status: i32::from_be_bytes(status),
                });
            }
            other => bail!("unknown runner frame type: {other}"),
        }
    }
}

fn terminal_size() -> TerminalSize {
    let output = Command::new("stty")
        .args(["-f", "/dev/tty", "size"])
        .output();
    let Ok(output) = output else {
        return TerminalSize { rows: 24, cols: 80 };
    };
    if !output.status.success() {
        return TerminalSize { rows: 24, cols: 80 };
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.split_whitespace();
    let rows = fields
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(24);
    let cols = fields
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(80);
    TerminalSize { rows, cols }
}

fn set_terminal_raw() -> Result<()> {
    run(Command::new("stty").args(["-f", "/dev/tty", "raw", "-echo", "min", "0", "time", "1"]))
        .context("stty raw")
}

fn write_stdout_frame(stream: &mut UnixStream, len: u32) -> Result<()> {
    let mut remaining = len as usize;
    let mut buf = [0u8; 8192];
    let mut stdout = std::io::stdout().lock();
    while remaining > 0 {
        let chunk_len = remaining.min(buf.len());
        stream
            .read_exact(&mut buf[..chunk_len])
            .context("read runner output frame")?;
        stdout
            .write_all(&buf[..chunk_len])
            .context("write command output")?;
        remaining -= chunk_len;
    }
    stdout.flush().context("flush command output")?;
    Ok(())
}

fn connect_unix(path: &Path, timeout: Duration) -> Result<UnixStream> {
    let start = Instant::now();
    let mut last_error = None;
    while start.elapsed() < timeout {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                last_error = Some(e);
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
    match last_error {
        Some(e) => Err(e).with_context(|| format!("timed out connecting to {}", path.display())),
        None => bail!("timed out connecting to {}", path.display()),
    }
}

fn spawn_snapshot_after_command(
    ctx: u32,
    snapshot: PathBuf,
    terminal_state: Option<String>,
    command_done: Arc<AtomicBool>,
    done: mpsc::Receiver<Result<CommandResult, String>>,
) {
    thread::spawn(move || {
        let command_result = match done.recv() {
            Ok(Ok(result)) => {
                command_done.store(true, Ordering::SeqCst);
                result
            }
            Ok(Err(e)) => {
                command_done.store(true, Ordering::SeqCst);
                restore_terminal(&terminal_state);
                eprintln!("{e}");
                std::process::exit(1);
            }
            Err(e) => {
                command_done.store(true, Ordering::SeqCst);
                restore_terminal(&terminal_state);
                eprintln!("command client failed: {e}");
                std::process::exit(1);
            }
        };
        thread::sleep(Duration::from_millis(250));
        let snapshot_c = cstring_path(&snapshot).unwrap_or_else(|e| {
            restore_terminal(&terminal_state);
            eprintln!("{e:#}");
            std::process::exit(1);
        });
        let started = Instant::now();
        let rc = unsafe { libkrun::krun_snapshot(ctx, snapshot_c.as_ptr()) };
        let snapshot_ms = started.elapsed().as_millis();
        if rc != 0 {
            restore_terminal(&terminal_state);
            eprintln!("libkrun::krun_snapshot failed: {}", os_error(rc));
            std::process::exit(1);
        }
        if let Err(e) = fs::write(snapshot.join("metadata"), SNAPSHOT_METADATA) {
            restore_terminal(&terminal_state);
            eprintln!("write snapshot metadata failed: {e:#}");
            std::process::exit(1);
        }
        if let Some(home) = dirs::home_dir() {
            let _ = fs::write(
                home.join(".libkrun/run/krun.snapshot_ms"),
                format!("{snapshot_ms}\n"),
            );
        }
        restore_terminal(&terminal_state);
        std::process::exit(command_result.exit_status);
    });
}

fn vm_exit_detail(rc: i32, console_output: &Path) -> String {
    let mut message = if rc == 0 {
        "VM exited before the command runner completed".to_string()
    } else {
        format!("libkrun::krun_start_enter failed: {}", os_error(rc))
    };
    message.push_str(&console_log_hint(console_output));
    message
}

fn console_log_hint(path: &Path) -> String {
    match console_log_excerpt(path) {
        Some(excerpt) => format!("\n\nVM console:\n{excerpt}"),
        None => String::new(),
    }
}

fn console_log_excerpt(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let start = bytes.len().saturating_sub(4096);
    let excerpt = String::from_utf8_lossy(&bytes[start..])
        .trim_end()
        .to_string();
    if excerpt.is_empty() {
        None
    } else {
        Some(excerpt)
    }
}

fn terminal_state() -> Option<String> {
    let output = Command::new("stty")
        .args(["-f", "/dev/tty", "-g"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn restore_terminal(state: &Option<String>) {
    if let Some(state) = state {
        let _ = Command::new("stty")
            .args(["-f", "/dev/tty", state])
            .status();
    }
}

fn cstring_path(path: &Path) -> Result<CString> {
    CString::new(path.to_string_lossy().as_bytes())
        .with_context(|| format!("path contains NUL: {}", path.display()))
}

fn run(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("run {:?}", command))?;
    if !status.success() {
        bail!("{:?} failed with {status}", command);
    }
    Ok(())
}

fn call_i32(rc: i32, name: &str) -> Result<()> {
    if rc < 0 {
        bail_krun(rc, name)?;
    }
    Ok(())
}

fn bail_krun<T>(rc: i32, name: &str) -> Result<T> {
    bail!("{name} failed: {}", os_error(rc))
}

fn os_error(rc: i32) -> String {
    if rc < 0 {
        std::io::Error::from_raw_os_error(-rc).to_string()
    } else {
        format!("unexpected return code {rc}")
    }
}
