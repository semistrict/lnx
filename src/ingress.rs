#[cfg(target_os = "macos")]
use std::os::fd::FromRawFd;
use std::{
    collections::HashMap,
    ffi::CString,
    fs,
    io::{BufReader, ErrorKind, Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream, UdpSocket},
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
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
use lnx_protocol::Message;
use rustls::{
    ServerConfig as TlsServerConfig, ServerConnection,
    crypto::aws_lc_rs::sign::any_supported_type,
    pki_types::CertificateDer,
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};

use crate::{paths::Layout, runner};

const DEFAULT_DOMAIN: &str = "lnx";
const DEFAULT_DNS_ADDR: &str = "127.0.0.1:5354";
const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:80";
const DEFAULT_HTTPS_ADDR: &str = "127.0.0.1:443";
const DEFAULT_SUBNET: &str = "192.168.106.0/24";
const AUTOSTART_IDLE_TTL_MS: &str = "30000";
const CA_COMMON_NAME: &str = "lnx local ingress CA";
const SERVICE_LABEL: &str = "com.semistrict.lnx.ingress";
const LAUNCH_DAEMON_PATH: &str = "/Library/LaunchDaemons/com.semistrict.lnx.ingress.plist";
const LAUNCH_AGENT_NAME: &str = "com.semistrict.lnx.ingress.plist";
const CHROME_DEVTOOLS_PORT: u16 = 9222;

#[derive(Debug, Clone)]
pub struct Config {
    pub domain: String,
    pub dns_addr: String,
    pub http_addr: String,
    pub https_addr: String,
    pub subnet: String,
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
        subnet: env_or("LNX_INGRESS_SUBNET", DEFAULT_SUBNET),
        resolver_dir: PathBuf::from(env_or("LNX_INGRESS_RESOLVER_DIR", "/etc/resolver")),
        state_dir: PathBuf::from(env_or(
            "LNX_INGRESS_STATE_DIR",
            home.join(".lnx").join("ingress").to_string_lossy().as_ref(),
        )),
    })
}

pub fn enable(config: &Config) -> Result<()> {
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
    config: Config,
) -> Result<()> {
    if cleanup {
        let _ = fs::remove_file(config.resolver_path());
        let _ = fs::remove_file(config.socket_path());
        return Ok(());
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
    if unsafe { libc::geteuid() } != 0 {
        return None;
    }
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
            "LNX_INGRESS_SUBNET",
            "LNX_INGRESS_RESOLVER_DIR",
            "LNX_INGRESS_STATE_DIR",
            "LNX_INGRESS_USER",
            "LNX_VMNET_DEBUG",
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

fn install_service(config: &Config) -> Result<()> {
    fs::create_dir_all(&config.state_dir)
        .with_context(|| format!("create {}", config.state_dir.display()))?;
    ensure_user_owned_lnx_dirs(config);
    ensure_ca(config)?;
    if config.requires_privileged_service() {
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

fn uninstall_service(config: &Config) -> Result<()> {
    unload_service(config);
    let _ = fs::remove_file(config.launchd_path());
    let _ = fs::remove_file(config.resolver_path());
    let _ = fs::remove_file(config.socket_path());
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
    let exe = std::env::current_exe().context("current executable")?;
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
    <key>LNX_INGRESS_SUBNET</key>
    <string>{subnet}</string>
    <key>LNX_INGRESS_RESOLVER_DIR</key>
    <string>{resolver_dir}</string>
    <key>LNX_INGRESS_STATE_DIR</key>
    <string>{state_dir}</string>
    <key>LNX_INGRESS_USER</key>
    <string>{user}</string>{debug_env}
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
        subnet = xml_escape(&config.subnet),
        log = xml_escape(&config.log_path().display().to_string()),
        debug_env = match std::env::var("LNX_VMNET_DEBUG") {
            Ok(value) => format!(
                "\n    <key>LNX_VMNET_DEBUG</key>\n    <string>{}</string>",
                xml_escape(&value)
            ),
            Err(_) => String::new(),
        },
        resolver_dir = xml_escape(&config.resolver_dir.display().to_string()),
        state_dir = xml_escape(&config.state_dir.display().to_string()),
        user = xml_escape(&user),
    ))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
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

#[derive(Debug, Clone, Copy)]
struct AttachInfo {
    ip: Ipv4Addr,
    prefix: u8,
    gateway: Ipv4Addr,
}

/// Distinguishes a re-attach of the same instance so a stale keepalive
/// thread's detach cannot remove a newer attachment.
#[cfg(target_os = "macos")]
static ATTACH_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Routable per-VM networking, mirroring Apple's container tool: one
/// NAT-mode vmnet network with a dedicated subnet, DHCP disabled, and
/// addresses allocated here per instance. Lives in the ingress daemon
/// because creating vmnet interfaces requires root.
#[cfg(target_os = "macos")]
struct ActiveAttachment {
    generation: u64,
    ip: Ipv4Addr,
    // Dropped (stopping the vmnet interface) when removed from `active`.
    _attachment: crate::vmnet::Attachment,
}

#[cfg(target_os = "macos")]
struct NetworkService {
    network: Option<crate::vmnet::Network>,
    state_path: PathBuf,
    // Persisted per-instance address reservations (stable across restarts).
    allocations: std::sync::Mutex<HashMap<String, Ipv4Addr>>,
    // Live interfaces, keyed by instance. Removed when the owner exits, which
    // stops the interface and frees its pump thread.
    active: std::sync::Mutex<HashMap<String, ActiveAttachment>>,
}

#[cfg(target_os = "macos")]
impl NetworkService {
    fn start(config: &Config) -> Arc<NetworkService> {
        let state_path = config.state_dir.join("network.json");
        let allocations = Self::load_allocations(&state_path);
        let network = match crate::vmnet::parse_subnet(&config.subnet)
            .and_then(|(subnet, prefix)| crate::vmnet::Network::create(subnet, prefix))
        {
            Ok(network) => {
                eprintln!(
                    "vmnet network ready subnet={}/{} gateway={}",
                    network.subnet(),
                    network.prefix(),
                    network.gateway()
                );
                Some(network)
            }
            Err(e) => {
                eprintln!("vmnet network unavailable, VMs fall back to gvproxy NAT: {e:#}");
                None
            }
        };
        Arc::new(NetworkService {
            network,
            state_path,
            allocations: std::sync::Mutex::new(allocations),
            active: std::sync::Mutex::new(HashMap::new()),
        })
    }

    fn subnet_json(&self) -> String {
        match &self.network {
            Some(network) => format!("\"{}/{}\"", network.subnet(), network.prefix()),
            None => "null".to_string(),
        }
    }

    /// Names resolve only for instances with a live attachment, so a stopped
    /// or gvproxy-fallback VM returns NXDOMAIN rather than a black-hole IP.
    fn instance_ips(&self) -> HashMap<String, Ipv4Addr> {
        self.active
            .lock()
            .unwrap()
            .iter()
            .map(|(instance, active)| (instance.clone(), active.ip))
            .collect()
    }

    fn attach(&self, instance: &str) -> Result<(OwnedFd, AttachInfo, u64)> {
        let network = self
            .network
            .as_ref()
            .context("vmnet network is not available")?;
        let ip = self.allocate(instance, network)?;
        let mut attachment = network.attach(instance)?;
        let fd = attachment
            .take_guest_fd()
            .context("vmnet attachment has no guest fd")?;
        let generation = ATTACH_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Replacing an existing attachment stops the previous interface; the
        // bootstrap lock guarantees at most one live owner per instance.
        self.active.lock().unwrap().insert(
            instance.to_string(),
            ActiveAttachment {
                generation,
                ip,
                _attachment: attachment,
            },
        );
        Ok((
            fd,
            AttachInfo {
                ip,
                prefix: network.prefix(),
                gateway: network.gateway(),
            },
            generation,
        ))
    }

    /// Removes a live attachment (stopping its interface) if it is still the
    /// generation that the calling keepalive thread created.
    fn detach(&self, instance: &str, generation: u64) {
        let mut active = self.active.lock().unwrap();
        if active.get(instance).map(|a| a.generation) == Some(generation) {
            active.remove(instance);
        }
    }

    fn allocate(&self, instance: &str, network: &crate::vmnet::Network) -> Result<Ipv4Addr> {
        let mut allocations = self.allocations.lock().unwrap();
        if let Some(ip) = allocations.get(instance).copied() {
            if subnet_contains(network, ip) {
                return Ok(ip);
            }
            // The subnet changed under a persisted reservation; reallocate.
            allocations.remove(instance);
        }
        let subnet = u32::from(network.subnet());
        let broadcast = subnet | !u32::from(crate::vmnet::mask_for_prefix(network.prefix()));
        let used: std::collections::HashSet<Ipv4Addr> = allocations.values().copied().collect();
        // vmnet uses .0 as the host-side gateway; keep .1 reserved so
        // existing allocations and route stamps stay conservative.
        let ip = match (subnet + 2..broadcast)
            .map(Ipv4Addr::from)
            .find(|candidate| !used.contains(candidate))
        {
            Some(ip) => ip,
            None => {
                // Out of addresses: reclaim a reservation with no live VM
                // rather than failing, so churned instance names don't
                // exhaust the subnet.
                let active = self.active.lock().unwrap();
                let victim = allocations
                    .keys()
                    .find(|name| !active.contains_key(*name))
                    .cloned();
                drop(active);
                match victim.and_then(|name| allocations.remove(&name)) {
                    Some(ip) => ip,
                    None => bail!("subnet {} is out of addresses", network.subnet()),
                }
            }
        };
        allocations.insert(instance.to_string(), ip);
        if let Err(e) = Self::save_allocations(&self.state_path, &allocations) {
            eprintln!("failed to persist {}: {e:#}", self.state_path.display());
        }
        Ok(ip)
    }

    fn load_allocations(path: &Path) -> HashMap<String, Ipv4Addr> {
        let Ok(raw) = fs::read_to_string(path) else {
            return HashMap::new();
        };
        match serde_json::from_str::<HashMap<String, Ipv4Addr>>(&raw) {
            Ok(allocations) => allocations,
            Err(e) => {
                eprintln!("ignoring malformed {}: {e}", path.display());
                HashMap::new()
            }
        }
    }

    fn save_allocations(path: &Path, allocations: &HashMap<String, Ipv4Addr>) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let raw = serde_json::to_string_pretty(allocations).context("encode network state")?;
        // Atomic: a crash mid-write must not truncate the reservation file and
        // silently reassign every instance's address on the next start.
        let temp = path.with_extension("json.tmp");
        fs::write(&temp, raw).with_context(|| format!("write {}", temp.display()))?;
        fs::rename(&temp, path).with_context(|| format!("rename {}", path.display()))?;
        chown_to_ingress_user(path);
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn subnet_contains(network: &crate::vmnet::Network, ip: Ipv4Addr) -> bool {
    let subnet = u32::from(network.subnet());
    let broadcast = subnet | !u32::from(crate::vmnet::mask_for_prefix(network.prefix()));
    let ip = u32::from(ip);
    ip > subnet + 1 && ip < broadcast
}

/// Per-VM networking needs vmnet; elsewhere the daemon only serves DNS and
/// the HTTP ingress, and VMs keep their gvproxy NAT.
#[cfg(not(target_os = "macos"))]
struct NetworkService;

#[cfg(not(target_os = "macos"))]
impl NetworkService {
    fn start(_config: &Config) -> Arc<NetworkService> {
        Arc::new(NetworkService)
    }

    fn subnet_json(&self) -> String {
        "null".to_string()
    }

    fn instance_ips(&self) -> HashMap<String, Ipv4Addr> {
        HashMap::new()
    }

    fn attach(&self, _instance: &str) -> Result<(OwnedFd, AttachInfo, u64)> {
        anyhow::bail!("per-VM networking requires macOS")
    }

    fn detach(&self, _instance: &str, _generation: u64) {}
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

/// SCM_RIGHTS control buffer aligned for `cmsghdr`. A plain `[u8; N]` has
/// alignment 1, so reading/writing cmsghdr fields through it is UB.
#[repr(C, align(8))]
struct CmsgBuf([u8; 64]);

impl CmsgBuf {
    fn zeroed() -> Self {
        CmsgBuf([0u8; 64])
    }
}

fn send_bytes_with_fd(
    stream: &UnixStream,
    bytes: &[u8],
    fd: BorrowedFd<'_>,
) -> std::io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let mut control = CmsgBuf::zeroed();
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.0.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(4) } as _;
    let raw_fd: i32 = fd.as_raw_fd();
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(4) as _;
        std::ptr::copy_nonoverlapping(
            (&raw const raw_fd).cast::<u8>(),
            libc::CMSG_DATA(cmsg),
            std::mem::size_of::<i32>(),
        );
    }
    let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, 0) };
    if sent < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn recv_bytes_with_fd(
    stream: &UnixStream,
    buf: &mut [u8],
) -> std::io::Result<(usize, Option<OwnedFd>)> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut control = CmsgBuf::zeroed();
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.0.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(4) } as _;
    let received = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
    if received < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut fd = None;
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let mut raw_fd: i32 = -1;
                std::ptr::copy_nonoverlapping(
                    libc::CMSG_DATA(cmsg),
                    (&raw mut raw_fd).cast::<u8>(),
                    std::mem::size_of::<i32>(),
                );
                if raw_fd >= 0 {
                    fd = Some(OwnedFd::from_raw_fd(raw_fd));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Ok((received as usize, fd))
}

#[cfg(target_os = "macos")]
pub struct NetworkAttachment {
    pub fd: OwnedFd,
    pub ip: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Ipv4Addr,
    /// Held open for the VM's lifetime; the daemon detaches the interface and
    /// frees the address slot when this closes (owner exit).
    pub keepalive: UnixStream,
}

/// Asks the ingress daemon for a routable network attachment. Returns
/// Ok(None) when the daemon is not running or has no vmnet network, in
/// which case the VM falls back to gvproxy NAT.
#[cfg(target_os = "macos")]
pub fn request_network_attachment(
    config: &Config,
    instance: &str,
) -> Result<Option<NetworkAttachment>> {
    let Ok(mut stream) = UnixStream::connect(config.socket_path()) else {
        return Ok(None);
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .context("set ingress attach timeout")?;
    stream
        .write_all(
            format!("POST /network/attach?instance={instance} HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .as_bytes(),
        )
        .context("send ingress attach request")?;
    // The daemon replies once (fd + headers + JSON in a single message) and
    // then holds the connection open as a liveness channel, so read exactly
    // that one message rather than to EOF.
    let mut buf = vec![0u8; 4096];
    let (received, fd) = recv_bytes_with_fd(&stream, &mut buf).context("read ingress attach")?;
    let response = String::from_utf8_lossy(&buf[..received]).into_owned();
    if !response.starts_with("HTTP/1.1 200") {
        return Ok(None);
    }
    let Some(fd) = fd else {
        return Ok(None);
    };
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("");
    let ip: Ipv4Addr = json_field(body, "ip")
        .context("attach response missing ip")?
        .parse()
        .context("parse attach ip")?;
    let reported_gateway: Ipv4Addr = json_field(body, "gateway")
        .context("attach response missing gateway")?
        .parse()
        .context("parse attach gateway")?;
    let prefix: u8 = json_number_field(body, "prefix")
        .context("attach response missing prefix")?
        .parse()
        .context("parse attach prefix")?;
    let gateway = gateway_for_assigned_ip(ip, prefix).unwrap_or(reported_gateway);
    // No further reads; keep the stream solely as a liveness signal.
    let _ = stream.set_read_timeout(None);
    Ok(Some(NetworkAttachment {
        fd,
        ip,
        prefix,
        gateway,
        keepalive: stream,
    }))
}

#[cfg(target_os = "macos")]
fn gateway_for_assigned_ip(ip: Ipv4Addr, prefix: u8) -> Option<Ipv4Addr> {
    if !(8..=29).contains(&prefix) {
        return None;
    }
    let mask = u32::from(crate::vmnet::mask_for_prefix(prefix));
    let subnet = Ipv4Addr::from(u32::from(ip) & mask);
    Some(crate::vmnet::gateway_for_subnet(subnet))
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
        let body = format!(
            "{{\"enabled\":true,\"domain\":\"{}\",\"dns_addr\":\"{}\",\"http_addr\":\"{}\",\"https_addr\":\"{}\",\"resolver_path\":\"{}\",\"network\":{},\"pid\":{}}}",
            config.domain,
            config.dns_addr,
            config.http_addr,
            config.https_addr,
            config.resolver_path().display(),
            network.subnet_json(),
            std::process::id()
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

/// Attaches the instance to the vmnet network, sends the guest fd to the
/// requester, and holds the connection open as a liveness channel: when the
/// VM owner exits and the connection closes, the interface is torn down and
/// its address slot freed.
fn serve_network_attachment(stream: UnixStream, instance: String, network: Arc<NetworkService>) {
    match network.attach(&instance) {
        Ok((fd, info, generation)) => {
            let response = format!(
                "HTTP/1.1 200 OK\r\n\r\n{{\"ip\":\"{}\",\"prefix\":{},\"gateway\":\"{}\"}}",
                info.ip, info.prefix, info.gateway
            );
            if let Err(e) = send_bytes_with_fd(&stream, response.as_bytes(), fd.as_fd()) {
                eprintln!("network attach reply for {instance} failed: {e}");
                network.detach(&instance, generation);
                return;
            }
            // The fd is now duplicated into the owner; drop our copy.
            drop(fd);
            wait_for_peer_close(&stream);
            network.detach(&instance, generation);
        }
        Err(e) => {
            eprintln!("network attach for {instance} failed: {e:#}");
            let mut stream = stream;
            let _ = write_http_response(
                &mut stream,
                "503 Service Unavailable",
                "text/plain",
                format!("{e:#}\n").as_bytes(),
            );
        }
    }
}

/// Blocks until the peer closes the connection (the VM owner exits).
fn wait_for_peer_close(stream: &UnixStream) {
    let mut stream = stream;
    let mut buf = [0u8; 64];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(_) => continue,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
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
    runner::proxy_stream_to_guest(&broker_socket, stream, request, "127.0.0.1", route.port)
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
mod tests {
    use super::*;

    #[test]
    fn parses_ingress_hosts() {
        let route = parse_host("p8080-dev.lnx", "lnx").expect("parse");
        assert_eq!(route.instance, "dev");
        assert_eq!(route.port, 8080);

        let route = parse_host("p3000-parent-child.lnx:80", "lnx").expect("parse");
        assert_eq!(route.instance, "parent-child");
        assert_eq!(route.port, 3000);

        assert!(parse_host("p0-dev.lnx", "lnx").is_err());
        assert!(parse_host("8080-dev.lnx", "lnx").is_err());
        assert!(parse_host("p8080.lnx", "lnx").is_err());
        assert!(parse_host("p8080.dev.lnx", "lnx").is_err());
    }

    #[test]
    fn extracts_instance_from_bare_hosts() {
        assert_eq!(
            instance_from_host("dev.lnx", "lnx"),
            Some("dev".to_string())
        );
        assert_eq!(
            instance_from_host("Dev.LNX:443", "lnx"),
            Some("dev".to_string())
        );
        assert_eq!(instance_from_host("a.dev.lnx", "lnx"), None);
        assert_eq!(instance_from_host(".lnx", "lnx"), None);
        assert_eq!(instance_from_host("dev.local", "lnx"), None);
    }

    #[test]
    fn parses_attach_requests() {
        assert_eq!(
            attach_instance_from_request(
                "POST /network/attach?instance=dev-1 HTTP/1.1\r\nHost: localhost\r\n\r\n"
            ),
            Some("dev-1".to_string())
        );
        assert_eq!(
            attach_instance_from_request("POST /network/attach?instance= HTTP/1.1\r\n\r\n"),
            None
        );
        assert_eq!(
            attach_instance_from_request("POST /network/attach?instance=../etc HTTP/1.1\r\n\r\n"),
            None
        );
        assert_eq!(
            attach_instance_from_request("GET /status HTTP/1.1\r\n\r\n"),
            None
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn derives_vmnet_gateway_from_assigned_ip() {
        assert_eq!(
            gateway_for_assigned_ip(Ipv4Addr::new(192, 168, 106, 207), 24),
            Some(Ipv4Addr::new(192, 168, 106, 0))
        );
        assert_eq!(
            gateway_for_assigned_ip(Ipv4Addr::new(10, 42, 19, 10), 16),
            Some(Ipv4Addr::new(10, 42, 0, 0))
        );
        assert_eq!(
            gateway_for_assigned_ip(Ipv4Addr::new(10, 0, 0, 2), 31),
            None
        );
    }

    #[test]
    fn parses_json_number_fields() {
        assert_eq!(
            json_number_field("{\"prefix\":24,\"x\":\"y\"}", "prefix"),
            Some("24".to_string())
        );
        assert_eq!(json_number_field("{\"prefix\":\"24\"}", "prefix"), None);
        assert_eq!(json_number_field("{}", "prefix"), None);
    }

    fn dns_query(host: &str) -> Vec<u8> {
        let mut packet = vec![
            0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        for label in host.split('.') {
            packet.push(label.len() as u8);
            packet.extend_from_slice(label.as_bytes());
        }
        packet.push(0);
        packet.extend_from_slice(&1u16.to_be_bytes());
        packet.extend_from_slice(&1u16.to_be_bytes());
        packet
    }

    #[test]
    fn dns_answers_port_hosts_with_localhost_and_instances_with_their_ip() {
        let mut ips = HashMap::new();
        ips.insert("dev".to_string(), Ipv4Addr::new(192, 168, 106, 2));

        let response = dns_response(&dns_query("p8080-dev.lnx"), "lnx", &ips).expect("response");
        assert_eq!(&response[response.len() - 4..], &[127, 0, 0, 1]);

        let response = dns_response(&dns_query("dev.lnx"), "lnx", &ips).expect("response");
        assert_eq!(&response[response.len() - 4..], &[192, 168, 106, 2]);

        // NXDOMAIN: rcode 3, no answer records.
        let response = dns_response(&dns_query("other.lnx"), "lnx", &ips).expect("response");
        assert_eq!(response[3], 0x03);
        assert_eq!(&response[6..8], &[0, 0]);
    }

    #[test]
    fn parses_fork_query_from_request_target() {
        assert_eq!(
            fork_source_from_request_target("/vnc.html?lnx:fork=foo"),
            Some("foo".to_string())
        );
        assert_eq!(
            fork_source_from_request_target("/vnc.html?a=1&lnx:fork=source%2Evm&b=2"),
            Some("source.vm".to_string())
        );
        assert_eq!(fork_source_from_request_target("/vnc.html?a=1"), None);
        assert_eq!(fork_source_from_request_target("/vnc.html?lnx:fork="), None);
    }

    #[test]
    fn removes_only_fork_query_param_for_redirect() {
        assert_eq!(clean_fork_request_target("/?lnx:fork=foo"), "/");
        assert_eq!(
            clean_fork_request_target("/vnc.html?a=1&lnx:fork=foo&b=2"),
            "/vnc.html?a=1&b=2"
        );
        assert_eq!(
            clean_fork_request_target("/vnc.html?autoconnect=true"),
            "/vnc.html?autoconnect=true"
        );
    }

    #[test]
    fn extracts_request_target() {
        assert_eq!(
            request_target(b"GET /vnc.html?lnx:fork=foo HTTP/1.1\r\nHost: p6080.bar.lnx\r\n\r\n"),
            Some("/vnc.html?lnx:fork=foo")
        );
    }

    #[test]
    fn rewrites_chrome_devtools_host_header() {
        let request =
            b"GET /json/version HTTP/1.1\r\nHost: p9222-default.lnx\r\nConnection: close\r\n\r\n"
                .to_vec();

        assert_eq!(
            rewrite_proxy_request_host(request, 9222),
            b"GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:9222\r\nConnection: close\r\n\r\n"
                .to_vec()
        );
    }

    #[test]
    fn preserves_other_proxy_host_headers() {
        let request = b"GET / HTTP/1.1\r\nHost: p6080-default.lnx\r\n\r\n".to_vec();

        assert_eq!(rewrite_proxy_request_host(request.clone(), 6080), request);
    }
}
