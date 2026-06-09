use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream, UdpSocket},
    os::unix::{
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

use crate::{paths::Layout, runner};

const DEFAULT_DOMAIN: &str = "lnx";
const DEFAULT_DNS_ADDR: &str = "127.0.0.1:5354";
const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:80";
const SERVICE_LABEL: &str = "com.semistrict.lnx.ingress";
const LAUNCH_DAEMON_PATH: &str = "/Library/LaunchDaemons/com.semistrict.lnx.ingress.plist";
const LAUNCH_AGENT_NAME: &str = "com.semistrict.lnx.ingress.plist";

#[derive(Debug, Clone)]
pub struct Config {
    pub domain: String,
    pub dns_addr: String,
    pub http_addr: String,
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
        resolver_dir: PathBuf::from(env_or("LNX_INGRESS_RESOLVER_DIR", "/etc/resolver")),
        state_dir: PathBuf::from(env_or(
            "LNX_INGRESS_STATE_DIR",
            home.join(".lnx").join("ingress").to_string_lossy().as_ref(),
        )),
    })
}

pub fn enable(config: &Config) -> Result<()> {
    if status(config).is_ok() && service_loaded(config) {
        println!("ingress enabled for .{}", config.domain);
        return Ok(());
    }
    println!("writing {}", config.resolver_path().display());
    println!("starting dns on {}", config.dns_addr);
    println!("starting http on {}", config.http_addr);
    if config.needs_privileges() {
        println!(
            "lnx needs your password to install the macOS .{} resolver, register the launchd service, and listen on privileged local ports.",
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
    let log_path = config.log_path().display().to_string();
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
  <key>StandardOutPath</key>
  <string>{log_path}</string>
  <key>StandardErrorPath</key>
  <string>{log_path}</string>
</dict>
</plist>
"#,
        label = xml_escape(SERVICE_LABEL),
        exe = xml_escape(&exe.display().to_string()),
        home = xml_escape(&home),
        domain = xml_escape(&config.domain),
        dns_addr = xml_escape(&config.dns_addr),
        http_addr = xml_escape(&config.http_addr),
        resolver_dir = xml_escape(&config.resolver_dir.display().to_string()),
        state_dir = xml_escape(&config.state_dir.display().to_string()),
        user = xml_escape(&user),
        log_path = xml_escape(&log_path),
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
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config.log_path())
        .context("open ingress log")?;
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
    install_resolver(&config)?;

    let http_listener = TcpListener::bind(&config.http_addr)
        .with_context(|| format!("listen http {}", config.http_addr))?;
    http_listener
        .set_nonblocking(true)
        .context("set ingress http nonblocking")?;
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
        .with_context(|| format!("write {}", config.resolver_path().display()))
}

fn listen_admin(path: &PathBuf) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let _ = fs::remove_file(path);
    let listener =
        UnixListener::bind(path).with_context(|| format!("listen {}", path.display()))?;
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
            "{{\"enabled\":true,\"domain\":\"{}\",\"dns_addr\":\"{}\",\"http_addr\":\"{}\",\"resolver_path\":\"{}\",\"pid\":{}}}",
            config.domain,
            config.dns_addr,
            config.http_addr,
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
    let route = match parse_host(&host, &config.domain) {
        Ok(route) => route,
        Err(e) => {
            eprintln!("http route miss host={host:?}: {e:#}");
            write_http_response(&mut stream, "404 Not Found", "text/plain", b"not found\n")?;
            return Ok(());
        }
    };
    let layout = Layout::resolve(&route.instance, None, None)?;
    let broker_socket = layout.run_dir.join("broker.sock");
    ensure_instance_broker(&route.instance, &broker_socket, &config)?;
    runner::proxy_stream_to_guest(&broker_socket, stream, request, "127.0.0.1", route.port)
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
        .arg("--instance")
        .arg(instance)
        .arg("sleep")
        .arg("infinity")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(config.log_path())
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

pub fn parse_host(host: &str, domain: &str) -> Result<Route> {
    let host = strip_optional_port(host)
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let suffix = format!(".{}", domain.to_ascii_lowercase());
    if !host.ends_with(&suffix) {
        bail!("host {host:?} is not under .{domain}");
    }
    let name = host.trim_end_matches(&suffix);
    let labels = name.split('.').collect::<Vec<_>>();
    if labels.len() < 2 {
        bail!("host {host:?} must look like p<port>.<instance>.{domain}");
    }
    let port_label = labels[0];
    let Some(port) = port_label.strip_prefix('p') else {
        bail!("host {host:?} must start with p<port>");
    };
    let port = port.parse::<u16>().context("invalid ingress port")?;
    if port == 0 {
        bail!("invalid ingress port");
    }
    Ok(Route {
        instance: labels[1..].join("."),
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
        let route = parse_host("p8080.dev.lnx", "lnx").expect("parse");
        assert_eq!(route.instance, "dev");
        assert_eq!(route.port, 8080);

        let route = parse_host("p3000.parent.child.lnx:80", "lnx").expect("parse");
        assert_eq!(route.instance, "parent.child");
        assert_eq!(route.port, 3000);

        assert!(parse_host("p0.dev.lnx", "lnx").is_err());
        assert!(parse_host("8080.dev.lnx", "lnx").is_err());
        assert!(parse_host("p8080.lnx", "lnx").is_err());
    }
}
