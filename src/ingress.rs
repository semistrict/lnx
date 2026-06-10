use std::{
    ffi::CString,
    fs,
    io::{BufReader, ErrorKind, Read, Write},
    net::{TcpListener, TcpStream, UdpSocket},
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
const AUTOSTART_IDLE_TTL_MS: &str = "30000";
const CA_COMMON_NAME: &str = "lnx local ingress CA";
const SERVICE_LABEL: &str = "com.semistrict.lnx.ingress";
const LAUNCH_DAEMON_PATH: &str = "/Library/LaunchDaemons/com.semistrict.lnx.ingress.plist";
const LAUNCH_AGENT_NAME: &str = "com.semistrict.lnx.ingress.plist";

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
    if status(config).is_ok_and(|status| status.https_addr == config.https_addr)
        && service_loaded(config)
        && config.ca_cert_path().exists()
    {
        println!("ingress enabled for .{}", config.domain);
        return Ok(());
    }
    ensure_ca(config)?;
    ensure_wildcard_cert(config)?;
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
        for name in ["instances", "images"] {
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
    command.status().context("start ingress helper")?;
    Ok(())
}

fn install_service(config: &Config) -> Result<()> {
    fs::create_dir_all(&config.state_dir)
        .with_context(|| format!("create {}", config.state_dir.display()))?;
    ensure_user_owned_lnx_dirs(config);
    ensure_ca(config)?;
    if config.requires_privileged_service() {
        trust_ca(config)?;
    }
    if let Some(parent) = config.launchd_path().parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if stop(config).is_ok() {
        let _ = wait_for_stop(config, Duration::from_secs(5));
    }
    fs::write(config.launchd_path(), launchd_plist(config)?)
        .with_context(|| format!("write {}", config.launchd_path().display()))?;
    unload_service(config);
    run_launchctl(&[
        "bootstrap",
        &config.launchd_domain(),
        &config.launchd_path_string(),
    ])
    .context("bootstrap ingress launchd service")?;
    let target = format!("{}/{}", config.launchd_domain(), SERVICE_LABEL);
    let _ = run_launchctl(&["kickstart", "-k", &target]);
    Ok(())
}

fn uninstall_service(config: &Config) -> Result<()> {
    unload_service(config);
    let _ = fs::remove_file(config.launchd_path());
    let _ = fs::remove_file(config.resolver_path());
    let _ = fs::remove_file(config.socket_path());
    let _ = untrust_ca(config);
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

fn service_loaded(config: &Config) -> bool {
    let target = format!("{}/{}", config.launchd_domain(), SERVICE_LABEL);
    run_launchctl_quiet(&["print", &target]).is_ok()
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
    <key>LNX_INGRESS_RESOLVER_DIR</key>
    <string>{resolver_dir}</string>
    <key>LNX_INGRESS_STATE_DIR</key>
    <string>{state_dir}</string>
    <key>LNX_INGRESS_USER</key>
    <string>{user}</string>
  </dict>
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

    let stop = Arc::new(AtomicBool::new(false));
    let dns_stop = Arc::clone(&stop);
    let dns_domain = config.domain.clone();
    thread::spawn(move || serve_dns(dns, dns_domain, dns_stop));

    while !stop.load(Ordering::SeqCst) {
        match admin.accept() {
            Ok((stream, _)) => handle_admin(stream, &config, Arc::clone(&stop)),
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
    let _ = fs::set_permissions(
        path,
        <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o666),
    );
    Ok(listener)
}

fn handle_admin(mut stream: UnixStream, config: &Config, stop: Arc<AtomicBool>) {
    let mut buf = [0u8; 512];
    let n = stream.read(&mut buf).unwrap_or(0);
    let request = String::from_utf8_lossy(&buf[..n]);
    if request.starts_with("GET /status ") {
        let body = format!(
            "{{\"enabled\":true,\"domain\":\"{}\",\"dns_addr\":\"{}\",\"http_addr\":\"{}\",\"https_addr\":\"{}\",\"resolver_path\":\"{}\",\"pid\":{}}}",
            config.domain,
            config.dns_addr,
            config.http_addr,
            config.https_addr,
            config.resolver_path().display(),
            std::process::id()
        );
        let _ = write_http_response(&mut stream, "200 OK", "application/json", body.as_bytes());
    } else if request.starts_with("POST /stop ") {
        stop.store(true, Ordering::SeqCst);
        let _ = write_http_response(&mut stream, "204 No Content", "text/plain", b"");
    } else {
        let _ = write_http_response(&mut stream, "404 Not Found", "text/plain", b"not found\n");
    }
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

fn ensure_ca(config: &Config) -> Result<()> {
    fs::create_dir_all(config.ca_dir())
        .with_context(|| format!("create {}", config.ca_dir().display()))?;
    chown_to_ingress_user(&config.ca_dir());
    if config.ca_cert_path().exists() && config.ca_key_path().exists() {
        return Ok(());
    }
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
        ensure_wildcard_cert(&self.config)
            .and_then(|(cert, key)| load_certified_key(&cert, &key))
            .ok()
            .map(Arc::new)
    }
}

fn ensure_wildcard_cert(config: &Config) -> Result<(PathBuf, PathBuf)> {
    fs::create_dir_all(config.cert_dir())
        .with_context(|| format!("create {}", config.cert_dir().display()))?;
    chown_to_ingress_user(&config.cert_dir());
    let domain = config.domain.to_ascii_lowercase();
    let host = format!("*.{domain}");
    let safe = format!("wildcard.{domain}");
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
        let mut broker = runner::connect_broker(broker_socket)?;
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
    runner::proxy_stream_to_guest(&broker_socket, stream, request, "127.0.0.1", route.port)
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
    if broker_accepts_connections(broker_socket) {
        return Ok(());
    }
    start_instance(instance, config)?;
    wait_for_broker(broker_socket, Duration::from_secs(30))
        .with_context(|| format!("start lnx instance {instance}"))
}

fn broker_accepts_connections(broker_socket: &PathBuf) -> bool {
    UnixStream::connect(broker_socket).is_ok()
}

fn wait_for_broker(broker_socket: &PathBuf, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if broker_accepts_connections(broker_socket) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    bail!("timed out waiting for {}", broker_socket.display())
}

fn start_instance(instance: &str, config: &Config) -> Result<()> {
    let exe = std::env::current_exe().context("current executable")?;
    let mut command;
    if unsafe { libc::geteuid() } == 0 {
        if let Ok(user) = std::env::var("LNX_INGRESS_USER").or_else(|_| std::env::var("SUDO_USER"))
        {
            command = Command::new("sudo");
            command.arg("-u").arg(user);
            command.arg(format!(
                "HOME={}",
                std::env::var("HOME").unwrap_or_default()
            ));
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
            command.arg(format!(
                "HOME={}",
                std::env::var("HOME").unwrap_or_default()
            ));
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

fn serve_dns(socket: UdpSocket, domain: String, stop: Arc<AtomicBool>) {
    let mut buf = [0u8; 1500];
    while !stop.load(Ordering::SeqCst) {
        match socket.recv_from(&mut buf) {
            Ok((n, addr)) => {
                if let Some(response) = dns_response(&buf[..n], &domain) {
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

fn dns_response(packet: &[u8], domain: &str) -> Option<Vec<u8>> {
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
    let valid = parse_host(&name, domain).is_ok() && qtype == 1 && qclass == 1;

    let mut response = Vec::new();
    response.extend_from_slice(&packet[0..2]);
    response.extend_from_slice(if valid { &[0x84, 0x00] } else { &[0x84, 0x03] });
    response.extend_from_slice(&1u16.to_be_bytes());
    response.extend_from_slice(&(if valid { 1u16 } else { 0u16 }).to_be_bytes());
    response.extend_from_slice(&0u16.to_be_bytes());
    response.extend_from_slice(&0u16.to_be_bytes());
    response.extend_from_slice(question);
    if valid {
        response.extend_from_slice(&[0xc0, 0x0c]);
        response.extend_from_slice(&1u16.to_be_bytes());
        response.extend_from_slice(&1u16.to_be_bytes());
        response.extend_from_slice(&1u32.to_be_bytes());
        response.extend_from_slice(&4u16.to_be_bytes());
        response.extend_from_slice(&[127, 0, 0, 1]);
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
}
