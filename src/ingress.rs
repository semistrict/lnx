use std::{
    collections::HashMap,
    ffi::CString,
    fs,
    io::{BufReader, ErrorKind, Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream, UdpSocket},
    os::fd::AsRawFd,
    os::unix::{
        ffi::OsStrExt,
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use lnx_protocol::{Message, PROTOCOL_VERSION};
use rustls::{
    ServerConfig as TlsServerConfig, ServerConnection,
    crypto::ring::sign::any_supported_type,
    pki_types::CertificateDer,
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};

use crate::{paths::Layout, runner};

const DEFAULT_DOMAIN: &str = "lnx";
const DEFAULT_DNS_ADDR: &str = "127.0.0.1:5354";
const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:80";
const DEFAULT_HTTPS_ADDR: &str = "127.0.0.1:443";
const AUTOSTART_IDLE_TTL_MS: &str = "30000";
const CA_COMMON_NAME: &str = "lnx local ingress CA";
const SERVICE_LABEL: &str = "com.semistrict.lnx.ingress";
const LAUNCH_DAEMON_PATH: &str = "/Library/LaunchDaemons/com.semistrict.lnx.ingress.plist";
const LAUNCH_AGENT_NAME: &str = "com.semistrict.lnx.ingress.plist";
const SYSTEM_HELPER_PATH: &str = "/usr/local/libexec/lnx/lnx-ingress";
const CHROME_DEVTOOLS_PORT: u16 = 9222;

#[derive(Debug, Clone)]
pub struct Config {
    pub domain: String,
    pub dns_addr: String,
    pub http_addr: String,
    pub https_addr: String,
    pub resolver_dir: PathBuf,
    pub state_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub instance: String,
    pub port: u16,
}

pub fn load_config() -> Result<Config> {
    let home = dirs::home_dir().context("could not resolve home directory")?;
    Ok(Config {
        domain: env_or("LNX_INGRESS_DOMAIN", DEFAULT_DOMAIN),
        dns_addr: env_or("LNX_INGRESS_DNS_ADDR", DEFAULT_DNS_ADDR),
        http_addr: env_or("LNX_INGRESS_HTTP_ADDR", DEFAULT_HTTP_ADDR),
        https_addr: env_or("LNX_INGRESS_HTTPS_ADDR", DEFAULT_HTTPS_ADDR),
        resolver_dir: PathBuf::from(env_or("LNX_INGRESS_RESOLVER_DIR", "/etc/resolver")),
        state_dir: PathBuf::from(env_or(
            "LNX_INGRESS_STATE_DIR",
            home.join(".lnx").join("ingress").to_string_lossy().as_ref(),
        )),
    })
}

pub fn enable(config: &Config) -> Result<()> {
    if config.needs_privileges() {
        ensure_sudo_can_prompt_or_is_cached()?;
    }
    regenerate_ca(config)?;
    println!("writing {}", config.resolver_path().display());
    println!("starting dns on {}", config.dns_addr);
    println!("starting http on {}", config.http_addr);
    println!("starting https on {}", config.https_addr);
    println!("installing local CA {}", config.ca_cert_path().display());
    if config.needs_privileges() {
        println!(
            "lnx needs your password to install the macOS .{} resolver, trust the local lnx HTTPS CA, register the launchd service, and listen on privileged local ports.",
            config.domain
        );
    }
    start_helper(config, &["_ingress", "--install-service"])?;
    wait_for_status(config, Duration::from_secs(10))?;
    println!("ingress enabled for .{}", config.domain);
    Ok(())
}

pub fn disable(config: &Config) -> Result<()> {
    let mut stopped = false;
    if config.resolver_path().exists() {
        println!("removing {}", config.resolver_path().display());
    }
    if stop(config).is_ok() {
        stopped = true;
        let _ = wait_for_stop(config, Duration::from_secs(5));
    }
    let _ = start_helper(config, &["_ingress", "--uninstall-service"]);
    if stopped || !config.resolver_path().exists() {
        println!("ingress disabled");
    } else {
        println!("ingress already disabled");
    }
    Ok(())
}

pub fn print_status(config: &Config) -> Result<()> {
    match status(config) {
        Ok(status) => {
            println!("enabled");
            println!("domain: .{}", status.domain);
            println!("dns: {}", status.dns_addr);
            println!("http: {}", status.http_addr);
            println!("https: {}", status.https_addr);
            println!("resolver: {}", status.resolver_path);
            println!(
                "network: {}",
                status.network.as_deref().unwrap_or("disabled")
            );
            if let Some(error) = &status.network_error {
                println!("network-error: {error}");
            }
            match status.protocol_version {
                Some(version) => println!("protocol: {version}"),
                None => println!("protocol: stale"),
            }
            if let Some(binary) = &status.binary_path {
                println!("binary: {binary}");
            }
            if let Some(binary_status) = privileged_service_status(config, &status)? {
                println!("binary-status: {binary_status}");
            }
        }
        Err(_) => println!("disabled"),
    }
    Ok(())
}

pub fn run_hidden(
    spawn: bool,
    cleanup: bool,
    install_service_flag: bool,
    uninstall_service_flag: bool,
    refresh_if_running: bool,
    config: Config,
) -> Result<()> {
    if cleanup {
        let _ = fs::remove_file(config.resolver_path());
        let _ = fs::remove_file(config.socket_path());
        return Ok(());
    }
    if refresh_if_running {
        return refresh_if_running_service(&config);
    }
    if uninstall_service_flag {
        return uninstall_service(&config);
    }
    if install_service_flag {
        return install_service(&config);
    }
    if spawn {
        return spawn_daemon(&config);
    }
    run_daemon(config)
}

impl Config {
    fn socket_path(&self) -> PathBuf {
        self.state_dir.join("ingress.sock")
    }

    fn log_path(&self) -> PathBuf {
        self.state_dir.join("ingress.log")
    }

    fn ca_dir(&self) -> PathBuf {
        self.state_dir.join("ca")
    }

    fn cert_dir(&self) -> PathBuf {
        self.state_dir.join("certs")
    }

    fn ca_cert_path(&self) -> PathBuf {
        self.ca_dir().join("lnx-ca.crt")
    }

    fn ca_key_path(&self) -> PathBuf {
        self.ca_dir().join("lnx-ca.key")
    }

    fn resolver_path(&self) -> PathBuf {
        self.resolver_dir.join(&self.domain)
    }

    fn resolver_contents(&self) -> Result<String> {
        let (host, port) = self
            .dns_addr
            .rsplit_once(':')
            .context("parse ingress dns address")?;
        Ok(format!("nameserver {host}\nport {port}\n"))
    }

    fn needs_privileges(&self) -> bool {
        if unsafe { libc::geteuid() } == 0 {
            return false;
        }
        self.requires_privileged_service()
    }

    fn requires_privileged_service(&self) -> bool {
        is_privileged_addr(&self.http_addr)
            || is_privileged_addr(&self.https_addr)
            || is_privileged_addr(&self.dns_addr)
            || self.resolver_dir == PathBuf::from("/etc/resolver")
    }

    fn launchd_path(&self) -> PathBuf {
        if self.requires_privileged_service() {
            PathBuf::from(LAUNCH_DAEMON_PATH)
        } else {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Library")
                .join("LaunchAgents")
                .join(LAUNCH_AGENT_NAME)
        }
    }

    fn launchd_domain(&self) -> String {
        if self.requires_privileged_service() {
            "system".to_string()
        } else {
            format!("gui/{}", unsafe { libc::getuid() })
        }
    }

    fn launchd_path_string(&self) -> String {
        self.launchd_path().display().to_string()
    }
}

#[derive(Debug)]
struct Status {
    domain: String,
    dns_addr: String,
    http_addr: String,
    https_addr: String,
    resolver_path: String,
    network: Option<String>,
    network_error: Option<String>,
    protocol_version: Option<u16>,
    binary_path: Option<String>,
}

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| fallback.to_string())
}

fn is_privileged_addr(addr: &str) -> bool {
    addr.rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
        .map(|port| port < 1024)
        .unwrap_or(true)
}

fn ingress_user_ids() -> Option<(u32, u32)> {
    if let (Ok(uid), Ok(gid)) = (std::env::var("SUDO_UID"), std::env::var("SUDO_GID")) {
        if let (Ok(uid), Ok(gid)) = (uid.parse::<u32>(), gid.parse::<u32>()) {
            return Some((uid, gid));
        }
    }
    let user = std::env::var("LNX_INGRESS_USER")
        .or_else(|_| std::env::var("SUDO_USER"))
        .ok()?;
    let user = CString::new(user).ok()?;
    unsafe {
        let passwd = libc::getpwnam(user.as_ptr());
        if passwd.is_null() {
            None
        } else {
            Some(((*passwd).pw_uid, (*passwd).pw_gid))
        }
    }
}

fn ingress_user_name() -> Option<String> {
    std::env::var("LNX_INGRESS_USER")
        .or_else(|_| std::env::var("SUDO_USER"))
        .ok()
        .filter(|user| !user.is_empty())
}

fn chown_to_ingress_user(path: &Path) {
    let Some((uid, gid)) = ingress_user_ids() else {
        return;
    };
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    unsafe {
        libc::chown(path.as_ptr(), uid, gid);
    }
}

fn ensure_user_owned_lnx_dirs(config: &Config) {
    chown_to_ingress_user(&config.state_dir);
    if let Some(base) = config.state_dir.parent() {
        chown_to_ingress_user(base);
        for name in ["instances", "cache"] {
            let path = base.join(name);
            if fs::create_dir_all(&path).is_ok() {
                chown_to_ingress_user(&path);
            }
        }
    }
}

fn start_helper(config: &Config, args: &[&str]) -> Result<()> {
    let exe = std::env::current_exe().context("current executable")?;
    let mut command = if config.needs_privileges() {
        ensure_sudo_can_prompt_or_is_cached()?;
        let mut command = Command::new("sudo");
        command.arg(format!(
            "HOME={}",
            std::env::var("HOME").unwrap_or_default()
        ));
        for key in [
            "LNX_INGRESS_DOMAIN",
            "LNX_INGRESS_DNS_ADDR",
            "LNX_INGRESS_HTTP_ADDR",
            "LNX_INGRESS_HTTPS_ADDR",
            "LNX_INGRESS_RESOLVER_DIR",
            "LNX_INGRESS_STATE_DIR",
            "LNX_INGRESS_USER",
        ] {
            if let Ok(value) = std::env::var(key) {
                command.arg(format!("{key}={value}"));
            }
        }
        command.arg(exe);
        command
    } else {
        Command::new(exe)
    };
    command.args(args);
    let debug = format!("{command:?}");
    let status = command.status().context("start ingress helper")?;
    if !status.success() {
        bail!("{debug} failed with {status}");
    }
    Ok(())
}

fn ensure_sudo_can_prompt_or_is_cached() -> Result<()> {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 0 {
        return Ok(());
    }
    match Command::new("sudo")
        .arg("-n")
        .arg("true")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        _ => bail!(
            "installing ingress needs sudo from an interactive terminal; run `sudo lnx ingress enable` from a terminal"
        ),
    }
}

fn install_service(config: &Config) -> Result<()> {
    fs::create_dir_all(&config.state_dir)
        .with_context(|| format!("create {}", config.state_dir.display()))?;
    ensure_user_owned_lnx_dirs(config);
    ensure_ca(config)?;
    if config.requires_privileged_service() {
        install_system_helper()?;
        eprintln!("trusting local CA in System keychain");
        trust_ca(config)?;
    }
    if let Some(parent) = config.launchd_path().parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if stop(config).is_ok() {
        eprintln!("stopping existing ingress daemon");
        let _ = wait_for_stop(config, Duration::from_secs(5));
    }
    eprintln!(
        "writing launchd service {}",
        config.launchd_path().display()
    );
    fs::write(config.launchd_path(), launchd_plist(config)?)
        .with_context(|| format!("write {}", config.launchd_path().display()))?;
    eprintln!("bootstrapping launchd service {}", SERVICE_LABEL);
    unload_service(config);
    run_launchctl(&[
        "bootstrap",
        &config.launchd_domain(),
        &config.launchd_path_string(),
    ])
    .context("bootstrap ingress launchd service")?;
    let target = format!("{}/{}", config.launchd_domain(), SERVICE_LABEL);
    eprintln!("starting launchd service {target}");
    let _ = run_launchctl(&["kickstart", "-k", &target]);
    Ok(())
}

fn install_system_helper() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("installing the ingress helper requires root");
    }
    let source = std::env::current_exe().context("current executable")?;
    let dest = PathBuf::from(SYSTEM_HELPER_PATH);
    let parent = dest.parent().context("system helper path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        dest.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("lnx-ingress"),
        std::process::id()
    ));
    let copy_result = (|| -> Result<()> {
        fs::copy(&source, &temp)
            .with_context(|| format!("copy {} to {}", source.display(), temp.display()))?;
        fs::set_permissions(
            &temp,
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .with_context(|| format!("chmod {}", temp.display()))?;
        let temp_c =
            CString::new(temp.as_os_str().as_bytes()).context("helper path contains nul")?;
        unsafe {
            if libc::chown(temp_c.as_ptr(), 0, 0) != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("chown {}", temp.display()));
            }
        }
        fs::rename(&temp, &dest)
            .with_context(|| format!("install {} to {}", temp.display(), dest.display()))?;
        Ok(())
    })();
    if copy_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    copy_result?;
    eprintln!("installed ingress helper {}", dest.display());
    Ok(())
}

fn refresh_if_running_service(config: &Config) -> Result<()> {
    let Ok(status) = status(config) else {
        return Ok(());
    };
    if let Some(error) = privileged_service_binary_error(config, &status) {
        bail!("{error}");
    }
    if let Some(error) = privileged_service_helper_stale_error(config)? {
        bail!("{error}");
    }
    if status.protocol_version == Some(PROTOCOL_VERSION) {
        return Ok(());
    }
    eprintln!("refreshing stale ingress service");
    stop(config)?;
    let _ = wait_for_ready_status(config, Duration::from_secs(10))?;
    eprintln!("ingress service refreshed");
    Ok(())
}

fn uninstall_service(config: &Config) -> Result<()> {
    unload_service(config);
    let _ = fs::remove_file(config.launchd_path());
    let _ = fs::remove_file(config.resolver_path());
    let _ = fs::remove_file(config.socket_path());
    if config.requires_privileged_service() {
        let _ = fs::remove_file(SYSTEM_HELPER_PATH);
    }
    // Leave the CA trusted: re-trusting on the next enable would re-open the
    // Security auth dialog. The local dev CA persists like mkcert's.
    Ok(())
}

fn unload_service(config: &Config) {
    let _ = run_launchctl_quiet(&[
        "bootout",
        &config.launchd_domain(),
        &config.launchd_path_string(),
    ]);
    let target = format!("{}/{}", config.launchd_domain(), SERVICE_LABEL);
    let _ = run_launchctl_quiet(&["bootout", &target]);
}

fn run_launchctl(args: &[&str]) -> Result<()> {
    let status = Command::new("launchctl")
        .args(args)
        .status()
        .with_context(|| format!("launchctl {}", args.join(" ")))?;
    if !status.success() {
        bail!("launchctl {} failed with {status}", args.join(" "));
    }
    Ok(())
}

fn run_launchctl_quiet(args: &[&str]) -> Result<()> {
    let status = Command::new("launchctl")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("launchctl {}", args.join(" ")))?;
    if !status.success() {
        bail!("launchctl {} failed with {status}", args.join(" "));
    }
    Ok(())
}

fn launchd_plist(config: &Config) -> Result<String> {
    let exe = service_executable(config)?;
    let home = std::env::var("HOME").unwrap_or_default();
    let user = std::env::var("LNX_INGRESS_USER")
        .or_else(|_| std::env::var("SUDO_USER"))
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default();
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>_ingress</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key>
    <string>{home}</string>
    <key>LNX_INGRESS_DOMAIN</key>
    <string>{domain}</string>
    <key>LNX_INGRESS_DNS_ADDR</key>
    <string>{dns_addr}</string>
    <key>LNX_INGRESS_HTTP_ADDR</key>
    <string>{http_addr}</string>
    <key>LNX_INGRESS_HTTPS_ADDR</key>
    <string>{https_addr}</string>
    <key>LNX_INGRESS_RESOLVER_DIR</key>
    <string>{resolver_dir}</string>
    <key>LNX_INGRESS_STATE_DIR</key>
    <string>{state_dir}</string>
    <key>LNX_INGRESS_USER</key>
    <string>{user}</string>
  </dict>
  <key>StandardErrorPath</key>
  <string>{log}</string>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
</dict>
</plist>
"#,
        label = xml_escape(SERVICE_LABEL),
        exe = xml_escape(&exe.display().to_string()),
        home = xml_escape(&home),
        domain = xml_escape(&config.domain),
        dns_addr = xml_escape(&config.dns_addr),
        http_addr = xml_escape(&config.http_addr),
        https_addr = xml_escape(&config.https_addr),
        log = xml_escape(&config.log_path().display().to_string()),
        resolver_dir = xml_escape(&config.resolver_dir.display().to_string()),
        state_dir = xml_escape(&config.state_dir.display().to_string()),
        user = xml_escape(&user),
    ))
}

fn service_executable(config: &Config) -> Result<PathBuf> {
    if config.requires_privileged_service() {
        Ok(PathBuf::from(SYSTEM_HELPER_PATH))
    } else {
        std::env::current_exe().context("current executable")
    }
}

fn privileged_service_binary_error(config: &Config, status: &Status) -> Option<String> {
    privileged_service_binary_error_with_expected(config, status, Path::new(SYSTEM_HELPER_PATH))
}

fn privileged_service_binary_error_with_expected(
    config: &Config,
    status: &Status,
    expected: &Path,
) -> Option<String> {
    if !config.requires_privileged_service() {
        return None;
    }
    match status.binary_path.as_deref() {
        Some(binary) if Path::new(binary) == expected => None,
        Some(binary) => Some(format!(
            "ingress is running from {binary}; expected the system helper at {}; run `sudo lnx ingress enable` from a terminal",
            expected.display()
        )),
        None => Some(format!(
            "ingress status did not report its binary; expected the system helper at {}; run `sudo lnx ingress enable` from a terminal",
            expected.display()
        )),
    }
}

fn privileged_service_helper_stale_error(config: &Config) -> Result<Option<String>> {
    let current = std::env::current_exe().context("current executable")?;
    privileged_service_helper_stale_error_with_paths(
        config,
        &current,
        Path::new(SYSTEM_HELPER_PATH),
    )
}

fn privileged_service_helper_stale_error_with_paths(
    config: &Config,
    current: &Path,
    helper: &Path,
) -> Result<Option<String>> {
    if !config.requires_privileged_service() {
        return Ok(None);
    }
    if current == helper || !helper.exists() {
        return Ok(None);
    }
    if files_same_contents(&current, helper)? {
        return Ok(None);
    }
    Ok(Some(format!(
        "ingress system helper is stale: {} differs from {}; run `sudo lnx ingress enable` from a terminal",
        helper.display(),
        current.display()
    )))
}

fn privileged_service_status(config: &Config, status: &Status) -> Result<Option<String>> {
    let current = std::env::current_exe().context("current executable")?;
    privileged_service_status_with_paths(config, status, &current, Path::new(SYSTEM_HELPER_PATH))
}

fn privileged_service_status_with_paths(
    config: &Config,
    status: &Status,
    current: &Path,
    helper: &Path,
) -> Result<Option<String>> {
    if !config.requires_privileged_service() {
        return Ok(None);
    }
    if let Some(error) = privileged_service_binary_error_with_expected(config, status, helper) {
        return Ok(Some(format!("wrong-helper ({error})")));
    }
    if privileged_service_helper_stale_error_with_paths(config, current, helper)?.is_some() {
        return Ok(Some(
            "stale; run `sudo lnx ingress enable` from a terminal".to_string(),
        ));
    }
    Ok(Some("current".to_string()))
}

fn files_same_contents(left: &Path, right: &Path) -> Result<bool> {
    let left_meta = fs::metadata(left).with_context(|| format!("stat {}", left.display()))?;
    let right_meta = fs::metadata(right).with_context(|| format!("stat {}", right.display()))?;
    if left_meta.len() != right_meta.len() {
        return Ok(false);
    }
    let mut left = fs::File::open(left).with_context(|| format!("open {}", left.display()))?;
    let mut right = fs::File::open(right).with_context(|| format!("open {}", right.display()))?;
    let mut left_buf = [0u8; 64 * 1024];
    let mut right_buf = [0u8; 64 * 1024];
    loop {
        let left_n = left.read(&mut left_buf).context("read left file")?;
        let right_n = right.read(&mut right_buf).context("read right file")?;
        if left_n != right_n {
            return Ok(false);
        }
        if left_n == 0 {
            return Ok(true);
        }
        if left_buf[..left_n] != right_buf[..right_n] {
            return Ok(false);
        }
    }
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if ch.is_control() => escaped.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => escaped.push(ch),
        }
    }
    escaped
}

fn spawn_daemon(config: &Config) -> Result<()> {
    fs::create_dir_all(&config.state_dir)
        .with_context(|| format!("create {}", config.state_dir.display()))?;
    ensure_user_owned_lnx_dirs(config);
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config.log_path())
        .context("open ingress log")?;
    chown_to_ingress_user(&config.log_path());
    let err = log.try_clone().context("clone ingress log")?;
    let mut command = Command::new(std::env::current_exe().context("current executable")?);
    command
        .arg("_ingress")
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(err);
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _child = command.spawn().context("spawn ingress daemon")?;
    Ok(())
}

struct NetworkService;

impl NetworkService {
    fn start(_config: &Config) -> Arc<NetworkService> {
        Arc::new(NetworkService)
    }

    fn subnet_json(&self) -> String {
        "null".to_string()
    }

    fn error_json(&self) -> String {
        "null".to_string()
    }

    fn instance_ips(&self) -> HashMap<String, Ipv4Addr> {
        HashMap::new()
    }
}

fn valid_instance_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn attach_instance_from_request(request: &str) -> Option<String> {
    let target = request
        .strip_prefix("POST ")?
        .split_whitespace()
        .next()?
        .strip_prefix("/network/attach?instance=")?;
    // Normalize case so the allocation key matches DNS lookups, which are
    // lowercased.
    valid_instance_name(target).then(|| target.to_ascii_lowercase())
}

fn run_daemon(config: Config) -> Result<()> {
    fs::create_dir_all(&config.state_dir)
        .with_context(|| format!("create {}", config.state_dir.display()))?;
    ensure_user_owned_lnx_dirs(&config);
    install_resolver(&config)?;

    let http_listener = TcpListener::bind(&config.http_addr)
        .with_context(|| format!("listen http {}", config.http_addr))?;
    http_listener
        .set_nonblocking(true)
        .context("set ingress http nonblocking")?;
    let https_listener = TcpListener::bind(&config.https_addr)
        .with_context(|| format!("listen https {}", config.https_addr))?;
    https_listener
        .set_nonblocking(true)
        .context("set ingress https nonblocking")?;
    let tls_config = Arc::new(tls_server_config(&config)?);
    let dns = UdpSocket::bind(&config.dns_addr)
        .with_context(|| format!("listen dns {}", config.dns_addr))?;
    dns.set_read_timeout(Some(Duration::from_millis(250)))
        .context("set dns timeout")?;
    let admin = listen_admin(&config.socket_path())?;
    admin
        .set_nonblocking(true)
        .context("set ingress admin nonblocking")?;

    let network = NetworkService::start(&config);
    drop_ingress_privileges()?;

    let stop = Arc::new(AtomicBool::new(false));
    let dns_stop = Arc::clone(&stop);
    let dns_domain = config.domain.clone();
    let dns_network = Arc::clone(&network);
    thread::spawn(move || serve_dns(dns, dns_domain, dns_stop, dns_network));

    while !stop.load(Ordering::SeqCst) {
        match admin.accept() {
            Ok((stream, _)) => handle_admin(stream, &config, Arc::clone(&stop), &network),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }
        match http_listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let config = config.clone();
                thread::spawn(move || {
                    if let Err(e) = handle_http(stream, config) {
                        eprintln!("http error: {e:#}");
                    }
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }
        match https_listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let config = config.clone();
                let tls_config = Arc::clone(&tls_config);
                thread::spawn(move || {
                    if let Err(e) = handle_https(stream, config, tls_config) {
                        eprintln!("https error: {e:#}");
                    }
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = fs::remove_file(config.socket_path());
    let _ = fs::remove_file(config.resolver_path());
    Ok(())
}

fn install_resolver(config: &Config) -> Result<()> {
    fs::create_dir_all(&config.resolver_dir)
        .with_context(|| format!("create {}", config.resolver_dir.display()))?;
    fs::write(config.resolver_path(), config.resolver_contents()?)
        .with_context(|| format!("write {}", config.resolver_path().display()))?;
    Ok(())
}

fn listen_admin(path: &PathBuf) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let _ = fs::remove_file(path);
    let listener =
        UnixListener::bind(path).with_context(|| format!("listen {}", path.display()))?;
    chown_to_ingress_user(path);
    // Owner-only: the socket grants VM network fds, so restrict it to the
    // ingress user (and root) at the filesystem layer too.
    let _ = fs::set_permissions(
        path,
        <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
    );
    Ok(listener)
}

fn handle_admin(
    mut stream: UnixStream,
    config: &Config,
    stop: Arc<AtomicBool>,
    network: &Arc<NetworkService>,
) {
    let mut buf = [0u8; 512];
    let n = stream.read(&mut buf).unwrap_or(0);
    let request = String::from_utf8_lossy(&buf[..n]);
    if request.starts_with("GET /status ") {
        let binary_path = std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let body = format!(
            "{{\"enabled\":true,\"domain\":\"{}\",\"dns_addr\":\"{}\",\"http_addr\":\"{}\",\"https_addr\":\"{}\",\"resolver_path\":\"{}\",\"network\":{},\"network_error\":{},\"pid\":{},\"protocol_version\":{},\"binary_path\":\"{}\"}}",
            json_escape(&config.domain),
            json_escape(&config.dns_addr),
            json_escape(&config.http_addr),
            json_escape(&config.https_addr),
            json_escape(&config.resolver_path().display().to_string()),
            network.subnet_json(),
            network.error_json(),
            std::process::id(),
            PROTOCOL_VERSION,
            json_escape(&binary_path)
        );
        let _ = write_http_response(&mut stream, "200 OK", "application/json", body.as_bytes());
    } else if let Some(instance) = attach_instance_from_request(&request) {
        // Granting a VM's network fd to an arbitrary local user would let
        // them sniff or inject on its segment; only the owning user (or root)
        // may attach.
        if !peer_authorized(&stream) {
            let _ = write_http_response(&mut stream, "403 Forbidden", "text/plain", b"forbidden\n");
            return;
        }
        let _ = stream.set_nonblocking(false);
        let network = Arc::clone(network);
        thread::spawn(move || serve_network_attachment(stream, instance, network));
    } else if request.starts_with("POST /stop ") {
        if !peer_authorized(&stream) {
            let _ = write_http_response(&mut stream, "403 Forbidden", "text/plain", b"forbidden\n");
            return;
        }
        stop.store(true, Ordering::SeqCst);
        let _ = write_http_response(&mut stream, "204 No Content", "text/plain", b"");
    } else {
        let _ = write_http_response(&mut stream, "404 Not Found", "text/plain", b"not found\n");
    }
}

fn serve_network_attachment(
    mut stream: UnixStream,
    instance: String,
    _network: Arc<NetworkService>,
) {
    eprintln!("network attach for {instance} rejected: VM networking uses embedded gvproxy");
    let _ = write_http_response(
        &mut stream,
        "503 Service Unavailable",
        "text/plain",
        b"VM network attachments are not used; VM networking uses embedded gvproxy\n",
    );
}

/// The effective uid of the connected peer, used to authorize fd-granting
/// requests. The daemon runs as root, so only the owning user or root pass.
fn peer_authorized(stream: &UnixStream) -> bool {
    let Some(uid) = peer_uid(stream) else {
        return false;
    };
    let self_uid = unsafe { libc::geteuid() };
    // root, the user running the daemon, or the configured ingress user.
    uid == 0 || uid == self_uid || ingress_user_ids().map(|(u, _)| u) == Some(uid)
}

fn drop_ingress_privileges() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }
    let Some((uid, gid)) = ingress_user_ids() else {
        return Ok(());
    };
    if uid == 0 {
        return Ok(());
    }
    if let Some(user) = ingress_user_name() {
        let user = CString::new(user).context("ingress user contains nul")?;
        #[cfg(target_os = "macos")]
        let base_gid = libc::c_int::try_from(gid).context("ingress gid out of range")?;
        #[cfg(not(target_os = "macos"))]
        let base_gid = gid;
        unsafe {
            if libc::initgroups(user.as_ptr(), base_gid) != 0 {
                return Err(std::io::Error::last_os_error()).context("init ingress groups");
            }
        }
    }
    unsafe {
        if libc::setgid(gid) != 0 {
            return Err(std::io::Error::last_os_error()).context("drop ingress gid");
        }
        if libc::setuid(uid) != 0 {
            return Err(std::io::Error::last_os_error()).context("drop ingress uid");
        }
    }
    eprintln!("ingress dropped privileges to uid={uid} gid={gid}");
    Ok(())
}

#[cfg(target_os = "macos")]
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    (rc == 0).then_some(uid)
}

#[cfg(not(target_os = "macos"))]
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    (rc == 0).then_some(cred.uid)
}

fn status(config: &Config) -> Result<Status> {
    let response = admin_request(config, b"GET /status HTTP/1.1\r\nHost: localhost\r\n\r\n")?;
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("");
    Ok(Status {
        domain: json_field(body, "domain").unwrap_or_else(|| config.domain.clone()),
        dns_addr: json_field(body, "dns_addr").unwrap_or_else(|| config.dns_addr.clone()),
        http_addr: json_field(body, "http_addr").unwrap_or_else(|| config.http_addr.clone()),
        https_addr: json_field(body, "https_addr").unwrap_or_else(|| config.https_addr.clone()),
        resolver_path: json_field(body, "resolver_path")
            .unwrap_or_else(|| config.resolver_path().display().to_string()),
        network: json_field(body, "network"),
        network_error: json_field(body, "network_error"),
        protocol_version: json_number_field(body, "protocol_version")
            .and_then(|value| value.parse::<u16>().ok()),
        binary_path: json_field(body, "binary_path"),
    })
}

fn stop(config: &Config) -> Result<()> {
    let _ = admin_request(config, b"POST /stop HTTP/1.1\r\nHost: localhost\r\n\r\n")?;
    Ok(())
}

fn admin_request(config: &Config, request: &[u8]) -> Result<String> {
    let mut stream = UnixStream::connect(config.socket_path()).context("no ingress socket")?;
    stream.write_all(request)?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}

fn wait_for_status(config: &Config, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if status(config).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    bail!("timed out waiting for ingress")
}

fn wait_for_ready_status(config: &Config, timeout: Duration) -> Result<Status> {
    let deadline = Instant::now() + timeout;
    let mut last = None;
    while Instant::now() < deadline {
        match status(config) {
            Ok(status) if status.protocol_version == Some(PROTOCOL_VERSION) => return Ok(status),
            Ok(status) => last = Some(status),
            Err(_) => {}
        }
        thread::sleep(Duration::from_millis(100));
    }
    if let Some(status) = last {
        if status.protocol_version != Some(PROTOCOL_VERSION) {
            bail!("ingress restarted but is still running a stale protocol");
        }
    }
    bail!("timed out waiting for ingress")
}

fn wait_for_stop(config: &Config, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if status(config).is_err() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    bail!("timed out waiting for ingress stop")
}

fn json_field(body: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\":\"");
    let rest = body.split(&needle).nth(1)?;
    Some(rest.split('"').next()?.to_string())
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn json_number_field(body: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\":");
    let rest = body.split(&needle).nth(1)?;
    let value: String = rest.chars().take_while(char::is_ascii_digit).collect();
    (!value.is_empty()).then_some(value)
}

fn ensure_ca(config: &Config) -> Result<()> {
    fs::create_dir_all(config.ca_dir())
        .with_context(|| format!("create {}", config.ca_dir().display()))?;
    chown_to_ingress_user(&config.ca_dir());
    if config.ca_cert_path().exists() && config.ca_key_path().exists() {
        return Ok(());
    }
    generate_ca(config)
}

fn regenerate_ca(config: &Config) -> Result<()> {
    if config.ca_dir().exists() {
        fs::remove_dir_all(config.ca_dir())
            .with_context(|| format!("remove {}", config.ca_dir().display()))?;
    }
    if config.cert_dir().exists() {
        fs::remove_dir_all(config.cert_dir())
            .with_context(|| format!("remove {}", config.cert_dir().display()))?;
    }
    fs::create_dir_all(config.ca_dir())
        .with_context(|| format!("create {}", config.ca_dir().display()))?;
    chown_to_ingress_user(&config.ca_dir());
    generate_ca(config)
}

fn generate_ca(config: &Config) -> Result<()> {
    let key = config.ca_key_path();
    let cert = config.ca_cert_path();
    run_command(
        Command::new("openssl")
            .arg("genrsa")
            .arg("-out")
            .arg(&key)
            .arg("2048"),
    )
    .context("generate ingress CA key")?;
    run_command(
        Command::new("openssl")
            .arg("req")
            .arg("-x509")
            .arg("-new")
            .arg("-nodes")
            .arg("-key")
            .arg(&key)
            .arg("-sha256")
            .arg("-days")
            .arg("3650")
            .arg("-subj")
            .arg(format!("/CN={CA_COMMON_NAME}"))
            .arg("-out")
            .arg(&cert),
    )
    .context("generate ingress CA certificate")?;
    chown_to_ingress_user(&config.ca_key_path());
    chown_to_ingress_user(&config.ca_cert_path());
    Ok(())
}

fn trust_ca(config: &Config) -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    run_command(
        Command::new("security")
            .arg("add-trusted-cert")
            .arg("-d")
            .arg("-r")
            .arg("trustRoot")
            .arg("-k")
            .arg("/Library/Keychains/System.keychain")
            .arg(config.ca_cert_path()),
    )
    .context("trust ingress CA")
}

// Retained for an explicit teardown; normal disable leaves the CA trusted.
#[allow(dead_code)]
fn untrust_ca(_config: &Config) -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let _ = Command::new("security")
        .arg("delete-certificate")
        .arg("-c")
        .arg(CA_COMMON_NAME)
        .arg("/Library/Keychains/System.keychain")
        .status();
    Ok(())
}

fn run_command(command: &mut Command) -> Result<()> {
    let debug = format!("{command:?}");
    let status = command.status().with_context(|| format!("run {debug}"))?;
    if !status.success() {
        bail!("{debug} failed with {status}");
    }
    Ok(())
}

fn tls_server_config(config: &Config) -> Result<TlsServerConfig> {
    ensure_ca(config)?;
    Ok(TlsServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(IngressCertResolver {
            config: config.clone(),
        })))
}

#[derive(Debug)]
struct IngressCertResolver {
    config: Config,
}

impl ResolvesServerCert for IngressCertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let host = client_hello.server_name()?.to_ascii_lowercase();
        if parse_host(&host, &self.config.domain).is_err() {
            return None;
        }
        ensure_host_cert(&self.config, &host)
            .and_then(|(cert, key)| load_certified_key(&cert, &key))
            .ok()
            .map(Arc::new)
    }
}

// A wildcard cert `*.lnx` is a wildcard directly under what OpenSSL treats as
// a TLD, which curl/OpenSSL refuse to match. Mint an exact-host leaf instead,
// cached per host, so every requested name validates.
fn ensure_host_cert(config: &Config, host: &str) -> Result<(PathBuf, PathBuf)> {
    fs::create_dir_all(config.cert_dir())
        .with_context(|| format!("create {}", config.cert_dir().display()))?;
    chown_to_ingress_user(&config.cert_dir());
    let host = host.to_ascii_lowercase();
    let safe = host.replace(['*', '/', ':'], "_");
    let key = config.cert_dir().join(format!("{safe}.key"));
    let csr = config.cert_dir().join(format!("{safe}.csr"));
    let cert = config.cert_dir().join(format!("{safe}.crt"));
    let ext = config.cert_dir().join(format!("{safe}.ext"));
    let serial = config.ca_dir().join("lnx-ca.srl");
    if file_nonempty(&cert) && file_nonempty(&key) {
        return Ok((cert, key));
    }
    fs::write(
        &ext,
        format!("subjectAltName=DNS:{host}\nextendedKeyUsage=serverAuth\n"),
    )
    .with_context(|| format!("write {}", ext.display()))?;
    run_command(
        Command::new("openssl")
            .arg("genrsa")
            .arg("-out")
            .arg(&key)
            .arg("2048"),
    )
    .context("generate ingress host key")?;
    run_command(
        Command::new("openssl")
            .arg("req")
            .arg("-new")
            .arg("-key")
            .arg(&key)
            .arg("-subj")
            .arg(format!("/CN={host}"))
            .arg("-out")
            .arg(&csr),
    )
    .context("generate ingress host csr")?;
    run_command(
        Command::new("openssl")
            .arg("x509")
            .arg("-req")
            .arg("-in")
            .arg(&csr)
            .arg("-CA")
            .arg(config.ca_cert_path())
            .arg("-CAkey")
            .arg(config.ca_key_path())
            .arg("-CAcreateserial")
            .arg("-CAserial")
            .arg(&serial)
            .arg("-out")
            .arg(&cert)
            .arg("-days")
            .arg("825")
            .arg("-sha256")
            .arg("-extfile")
            .arg(&ext),
    )
    .context("sign ingress host certificate")?;
    let _ = fs::remove_file(csr);
    chown_to_ingress_user(&key);
    chown_to_ingress_user(&cert);
    chown_to_ingress_user(&ext);
    chown_to_ingress_user(&serial);
    Ok((cert, key))
}

fn file_nonempty(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0)
}

fn load_certified_key(cert_path: &Path, key_path: &Path) -> Result<CertifiedKey> {
    let cert_file =
        fs::File::open(cert_path).with_context(|| format!("open {}", cert_path.display()))?;
    let certs = rustls_pemfile::certs(&mut BufReader::new(cert_file))
        .collect::<std::result::Result<Vec<CertificateDer<'static>>, _>>()
        .with_context(|| format!("read {}", cert_path.display()))?;
    let key_file =
        fs::File::open(key_path).with_context(|| format!("open {}", key_path.display()))?;
    let key = rustls_pemfile::private_key(&mut BufReader::new(key_file))
        .with_context(|| format!("read {}", key_path.display()))?
        .context("missing private key")?;
    let signing_key = any_supported_type(&key).context("unsupported private key")?;
    Ok(CertifiedKey::new(certs, signing_key))
}

fn handle_https(
    mut stream: TcpStream,
    config: Config,
    tls_config: Arc<TlsServerConfig>,
) -> Result<()> {
    let mut conn = ServerConnection::new(tls_config).context("create tls server connection")?;
    while conn.is_handshaking() {
        conn.complete_io(&mut stream).context("tls handshake")?;
    }

    let mut request = Vec::new();
    let mut buf = [0u8; 8192];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") && request.len() < 1024 * 1024 {
        match conn.reader().read(&mut buf) {
            Ok(0) => {
                conn.complete_io(&mut stream).context("read tls request")?;
            }
            Ok(n) => request.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                conn.complete_io(&mut stream).context("read tls request")?;
            }
            Err(e) => return Err(e).context("read tls plaintext"),
        }
    }

    let host = match request.split(|byte| *byte == b'\n').find_map(|line| {
        let line = String::from_utf8_lossy(line);
        line.trim_end_matches('\r')
            .strip_prefix("Host:")
            .map(str::trim)
            .map(ToOwned::to_owned)
    }) {
        Some(host) => host,
        None => {
            write_http_response(
                &mut conn.writer(),
                "400 Bad Request",
                "text/plain",
                b"missing Host\n",
            )?;
            flush_tls(&mut conn, &mut stream)?;
            return Ok(());
        }
    };

    if maybe_fork_and_redirect(&mut conn.writer(), &request, &host, &config)? {
        flush_tls(&mut conn, &mut stream)?;
        return Ok(());
    }

    let Some((broker_socket, route)) = route_http_host(&mut conn.writer(), &host, &config)? else {
        flush_tls(&mut conn, &mut stream)?;
        return Ok(());
    };
    let request = rewrite_proxy_request_host(request, route.port);
    flush_tls(&mut conn, &mut stream)?;
    proxy_tls_to_guest(stream, conn, &broker_socket, request, route.port)
}

fn proxy_tls_to_guest(
    mut stream: TcpStream,
    mut conn: ServerConnection,
    broker_socket: &Path,
    initial_bytes: Vec<u8>,
    guest_port: u16,
) -> Result<()> {
    let first_response_deadline = Instant::now() + Duration::from_secs(5);
    let (mut broker, channel_id, first_bytes) = 'connect: loop {
        let mut broker = connect_broker_retry(broker_socket, first_response_deadline)?;
        let channel_id = runner::new_request_id()?;
        runner::write_message(
            &mut broker,
            &Message::OpenTcp {
                channel_id,
                host: "127.0.0.1".to_string(),
                port: guest_port,
            },
        )?;
        if !initial_bytes.is_empty() {
            runner::write_message(
                &mut broker,
                &Message::Data {
                    channel_id,
                    bytes: initial_bytes.clone(),
                },
            )?;
        }
        loop {
            match runner::read_message(&mut broker)? {
                Message::Data {
                    channel_id: id,
                    bytes,
                } if id == channel_id => break 'connect (broker, channel_id, bytes),
                Message::Eof { channel_id: id } if id == channel_id => conn.send_close_notify(),
                Message::Close { channel_id: id } if id == channel_id => return Ok(()),
                Message::Error {
                    channel_id: id,
                    message,
                } if id == channel_id => {
                    if Instant::now() >= first_response_deadline {
                        bail!("{message}");
                    }
                    thread::sleep(Duration::from_millis(100));
                    break;
                }
                _ => {}
            }
        }
    };

    conn.writer().write_all(&first_bytes)?;
    flush_tls(&mut conn, &mut stream)?;
    stream
        .set_nonblocking(true)
        .context("set tls stream nonblocking")?;
    broker
        .set_read_timeout(Some(Duration::from_millis(10)))
        .context("set ingress broker timeout")?;

    let mut sent_client_eof = false;
    let mut buf = [0u8; 8192];
    loop {
        match conn.read_tls(&mut stream) {
            Ok(0) if !sent_client_eof => {
                sent_client_eof = true;
                let _ = runner::write_message(&mut broker, &Message::Eof { channel_id });
            }
            Ok(_) => {
                conn.process_new_packets().context("process tls packets")?;
                loop {
                    match conn.reader().read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => runner::write_message(
                            &mut broker,
                            &Message::Data {
                                channel_id,
                                bytes: buf[..n].to_vec(),
                            },
                        )?,
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(e) => return Err(e).context("read tls plaintext"),
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e).context("read tls"),
        }

        match runner::read_message(&mut broker) {
            Ok(Message::Data {
                channel_id: id,
                bytes,
            }) if id == channel_id => {
                conn.writer().write_all(&bytes)?;
            }
            Ok(Message::Eof { channel_id: id }) if id == channel_id => {
                conn.send_close_notify();
            }
            Ok(Message::Close { channel_id: id }) if id == channel_id => return Ok(()),
            Ok(Message::Error {
                channel_id: id,
                message,
            }) if id == channel_id => bail!("{message}"),
            Ok(_) => {}
            Err(e) if runner::is_timeout_error(&e) => {}
            Err(e) => return Err(e),
        }

        flush_tls_nonblocking(&mut conn, &mut stream)?;
        thread::sleep(Duration::from_millis(1));
    }
}

fn connect_broker_retry(socket: &Path, deadline: Instant) -> Result<UnixStream> {
    let mut last = None;
    while Instant::now() < deadline {
        match runner::connect_broker(socket) {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                last = Some(e);
                thread::sleep(Duration::from_millis(25));
            }
        }
    }
    match last {
        Some(e) => Err(e),
        None => bail!("timed out connecting to broker"),
    }
}

fn flush_tls(conn: &mut ServerConnection, stream: &mut TcpStream) -> Result<()> {
    while conn.wants_write() {
        conn.write_tls(stream).context("write tls")?;
    }
    Ok(())
}

fn flush_tls_nonblocking(conn: &mut ServerConnection, stream: &mut TcpStream) -> Result<()> {
    while conn.wants_write() {
        match conn.write_tls(stream) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e).context("write tls"),
        }
    }
    Ok(())
}

fn handle_http(mut stream: TcpStream, config: Config) -> Result<()> {
    let mut request = Vec::new();
    let mut buf = [0u8; 8192];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") && request.len() < 1024 * 1024 {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buf[..n]);
    }
    let host = match request.split(|byte| *byte == b'\n').find_map(|line| {
        let line = String::from_utf8_lossy(line);
        line.trim_end_matches('\r')
            .strip_prefix("Host:")
            .map(str::trim)
            .map(ToOwned::to_owned)
    }) {
        Some(host) => host,
        None => {
            write_http_response(
                &mut stream,
                "400 Bad Request",
                "text/plain",
                b"missing Host\n",
            )?;
            return Ok(());
        }
    };
    if maybe_fork_and_redirect(&mut stream, &request, &host, &config)? {
        return Ok(());
    }

    let Some((broker_socket, route)) = route_http_host(&mut stream, &host, &config)? else {
        return Ok(());
    };
    let request = rewrite_proxy_request_host(request, route.port);
    let proxy_stream = stream.try_clone().context("clone http proxy stream")?;
    match runner::proxy_stream_to_guest(
        &broker_socket,
        proxy_stream,
        request,
        "127.0.0.1",
        route.port,
    ) {
        Ok(()) => Ok(()),
        Err(e) => {
            let body = format!("bad gateway: {e:#}\n");
            let _ = write_http_response(
                &mut stream,
                "502 Bad Gateway",
                "text/plain",
                body.as_bytes(),
            );
            Err(e)
        }
    }
}

fn rewrite_proxy_request_host(request: Vec<u8>, guest_port: u16) -> Vec<u8> {
    if guest_port != CHROME_DEVTOOLS_PORT {
        return request;
    }
    rewrite_http_host_header(request, &format!("127.0.0.1:{guest_port}"))
}

fn rewrite_http_host_header(request: Vec<u8>, host: &str) -> Vec<u8> {
    let Some(headers_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
        return request;
    };

    let mut line_start = 0;
    while line_start < headers_end {
        let Some(line_len) = request[line_start..headers_end]
            .windows(2)
            .position(|window| window == b"\r\n")
        else {
            return request;
        };
        let line_end = line_start + line_len;
        if line_end >= line_start + 5
            && request[line_start..line_start + 5].eq_ignore_ascii_case(b"Host:")
        {
            let mut rewritten =
                Vec::with_capacity(request.len() + host.len().saturating_sub(line_len));
            rewritten.extend_from_slice(&request[..line_start]);
            rewritten.extend_from_slice(b"Host: ");
            rewritten.extend_from_slice(host.as_bytes());
            rewritten.extend_from_slice(&request[line_end..]);
            return rewritten;
        }
        line_start = line_end + 2;
    }

    request
}

fn maybe_fork_and_redirect(
    response: &mut impl Write,
    request: &[u8],
    host: &str,
    config: &Config,
) -> Result<bool> {
    let Some(request_target) = request_target(request) else {
        return Ok(false);
    };
    let Some(source_instance) = fork_source_from_request_target(request_target) else {
        return Ok(false);
    };
    let route = match parse_host(host, &config.domain) {
        Ok(route) => route,
        Err(_) => return Ok(false),
    };
    let dest = Layout::resolve(&route.instance, None, None)?;
    if !dest.rootfs.exists() {
        fork_instance(&source_instance, &route.instance, config)?;
    }
    write_redirect_response(response, &clean_fork_request_target(request_target))?;
    Ok(true)
}

fn route_http_host(
    response: &mut impl Write,
    host: &str,
    config: &Config,
) -> Result<Option<(PathBuf, Route)>> {
    let route = match parse_host(host, &config.domain) {
        Ok(route) => route,
        Err(e) => {
            eprintln!("ingress route miss host={host:?}: {e:#}");
            write_http_response(response, "404 Not Found", "text/plain", b"not found\n")?;
            return Ok(None);
        }
    };
    let layout = Layout::resolve(&route.instance, None, None)?;
    let broker_socket = layout.run_dir.join("broker.sock");
    ensure_instance_broker(&route.instance, &broker_socket, &config)?;
    Ok(Some((broker_socket, route)))
}

fn ensure_instance_broker(instance: &str, broker_socket: &PathBuf, config: &Config) -> Result<()> {
    if broker_accepts_connections(broker_socket).is_ok() {
        return Ok(());
    }
    start_instance(instance, config)?;
    wait_for_broker(broker_socket, Duration::from_secs(30))
        .with_context(|| format!("start lnx instance {instance}"))
}

fn broker_accepts_connections(broker_socket: &PathBuf) -> Result<()> {
    runner::connect_broker(broker_socket).map(|_| ())
}

fn wait_for_broker(broker_socket: &PathBuf, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last = None;
    while Instant::now() < deadline {
        match broker_accepts_connections(broker_socket) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
        thread::sleep(Duration::from_millis(100));
    }
    match last {
        Some(e) => {
            Err(e).with_context(|| format!("timed out waiting for {}", broker_socket.display()))
        }
        None => bail!("timed out waiting for {}", broker_socket.display()),
    }
}

fn start_instance(instance: &str, config: &Config) -> Result<()> {
    let exe = std::env::current_exe().context("current executable")?;
    let mut command;
    if unsafe { libc::geteuid() } == 0 {
        if let Ok(user) = std::env::var("LNX_INGRESS_USER").or_else(|_| std::env::var("SUDO_USER"))
        {
            command = Command::new("sudo");
            command.arg("-u").arg(user);
            if let Ok(home) = std::env::var("HOME") {
                command.arg(format!("HOME={home}"));
                command.current_dir(home);
            }
            command.arg(exe);
        } else {
            command = Command::new(exe);
        }
    } else {
        command = Command::new(exe);
    }
    command
        .env("LNX_BROKER_IDLE_TTL_MS", AUTOSTART_IDLE_TTL_MS)
        .arg("--instance")
        .arg(instance)
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(config.log_path())
                .inspect(|_| chown_to_ingress_user(&config.log_path()))
                .unwrap_or_else(|_| fs::File::create("/dev/null").expect("open /dev/null")),
        );
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _child = command
        .spawn()
        .with_context(|| format!("auto-start lnx instance {instance}"))?;
    Ok(())
}

fn fork_instance(source: &str, dest: &str, config: &Config) -> Result<()> {
    let exe = std::env::current_exe().context("current executable")?;
    let mut command;
    if unsafe { libc::geteuid() } == 0 {
        if let Ok(user) = std::env::var("LNX_INGRESS_USER").or_else(|_| std::env::var("SUDO_USER"))
        {
            command = Command::new("sudo");
            command.arg("-u").arg(user);
            if let Ok(home) = std::env::var("HOME") {
                command.arg(format!("HOME={home}"));
                command.current_dir(home);
            }
            command.arg(exe);
        } else {
            command = Command::new(exe);
        }
    } else {
        command = Command::new(exe);
    }
    let output = command
        .arg("--instance")
        .arg(source)
        .arg("fork")
        .arg(dest)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("fork lnx instance {dest} from {source}"))?;
    if !output.status.success() {
        bail!(
            "fork lnx instance {dest} from {source} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config.log_path())
        .ok();
    if log.is_some() {
        chown_to_ingress_user(&config.log_path());
    }
    if let Some(log) = &mut log {
        let _ = writeln!(log, "forked instance {dest} from {source}");
    }
    Ok(())
}

fn write_http_response(
    stream: &mut impl Write,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    Ok(())
}

fn write_redirect_response(stream: &mut impl Write, location: &str) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )?;
    Ok(())
}

fn request_target(request: &[u8]) -> Option<&str> {
    let line = request.split(|byte| *byte == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?.trim_end_matches('\r');
    let mut parts = line.split_whitespace();
    let _method = parts.next()?;
    parts.next()
}

fn fork_source_from_request_target(target: &str) -> Option<String> {
    let query = target.split_once('?')?.1;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "lnx:fork" && !value.is_empty() {
            return Some(percent_decode(value));
        }
    }
    None
}

fn clean_fork_request_target(target: &str) -> String {
    let Some((path, query)) = target.split_once('?') else {
        return target.to_string();
    };
    let kept = query
        .split('&')
        .filter(|pair| {
            let key = pair.split_once('=').map(|(key, _)| key).unwrap_or(*pair);
            key != "lnx:fork"
        })
        .filter(|pair| !pair.is_empty())
        .collect::<Vec<_>>();
    if kept.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", kept.join("&"))
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse_host(host: &str, domain: &str) -> Result<Route> {
    let host = strip_optional_port(host)
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let suffix = format!(".{}", domain.to_ascii_lowercase());
    if !host.ends_with(&suffix) {
        bail!("host {host:?} is not under .{domain}");
    }
    let name = host.trim_end_matches(&suffix);
    if name.contains('.') {
        bail!("host {host:?} must look like p<port>-<instance>.{domain}");
    }
    let Some((port_label, instance)) = name.split_once('-') else {
        bail!("host {host:?} must look like p<port>-<instance>.{domain}");
    };
    let Some(port) = port_label.strip_prefix('p') else {
        bail!("host {host:?} must start with p<port>-");
    };
    let port = port.parse::<u16>().context("invalid ingress port")?;
    if port == 0 || instance.is_empty() {
        bail!("invalid ingress port");
    }
    Ok(Route {
        instance: instance.to_string(),
        port,
    })
}

fn strip_optional_port(host: &str) -> &str {
    host.rsplit_once(':')
        .filter(|(_, port)| port.parse::<u16>().is_ok())
        .map(|(host, _)| host)
        .unwrap_or(host)
}

fn serve_dns(
    socket: UdpSocket,
    domain: String,
    stop: Arc<AtomicBool>,
    network: Arc<NetworkService>,
) {
    let mut buf = [0u8; 1500];
    while !stop.load(Ordering::SeqCst) {
        match socket.recv_from(&mut buf) {
            Ok((n, addr)) => {
                if let Some(response) = dns_response(&buf[..n], &domain, &network.instance_ips()) {
                    let _ = socket.send_to(&response, addr);
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
    }
}

/// The instance name for a bare `<instance>.<domain>` host, which resolves
/// to the instance's routable address when one is allocated. Port-labeled
/// `p<port>-<instance>` hosts keep resolving to the local ingress proxy.
fn instance_from_host(host: &str, domain: &str) -> Option<String> {
    let host = strip_optional_port(host)
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let suffix = format!(".{}", domain.to_ascii_lowercase());
    let name = host.strip_suffix(&suffix)?;
    (!name.is_empty() && !name.contains('.')).then(|| name.to_string())
}

fn dns_response(
    packet: &[u8],
    domain: &str,
    instance_ips: &HashMap<String, Ipv4Addr>,
) -> Option<Vec<u8>> {
    if packet.len() < 12 {
        return None;
    }
    let mut pos = 12;
    let mut labels = Vec::new();
    loop {
        let len = *packet.get(pos)? as usize;
        pos += 1;
        if len == 0 {
            break;
        }
        let label = packet.get(pos..pos + len)?;
        labels.push(String::from_utf8_lossy(label).to_string());
        pos += len;
    }
    let q_end = pos + 4;
    let question = packet.get(12..q_end)?;
    let qtype = u16::from_be_bytes([packet[pos], packet[pos + 1]]);
    let qclass = u16::from_be_bytes([packet[pos + 2], packet[pos + 3]]);
    let name = labels.join(".");
    let answer = if parse_host(&name, domain).is_ok() {
        Some(Ipv4Addr::LOCALHOST)
    } else {
        instance_from_host(&name, domain).and_then(|instance| instance_ips.get(&instance).copied())
    };
    let answer = answer.filter(|_| qtype == 1 && qclass == 1);

    let mut response = Vec::new();
    response.extend_from_slice(&packet[0..2]);
    response.extend_from_slice(if answer.is_some() {
        &[0x84, 0x00]
    } else {
        &[0x84, 0x03]
    });
    response.extend_from_slice(&1u16.to_be_bytes());
    response.extend_from_slice(&(if answer.is_some() { 1u16 } else { 0u16 }).to_be_bytes());
    response.extend_from_slice(&0u16.to_be_bytes());
    response.extend_from_slice(&0u16.to_be_bytes());
    response.extend_from_slice(question);
    if let Some(answer) = answer {
        response.extend_from_slice(&[0xc0, 0x0c]);
        response.extend_from_slice(&1u16.to_be_bytes());
        response.extend_from_slice(&1u16.to_be_bytes());
        response.extend_from_slice(&1u32.to_be_bytes());
        response.extend_from_slice(&4u16.to_be_bytes());
        response.extend_from_slice(&answer.octets());
    }
    Some(response)
}

#[cfg(test)]
mod tests;
