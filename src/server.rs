use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Seek, SeekFrom, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{
        Path as AxumPath, State,
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use futures_util::{SinkExt, StreamExt};
use lnx_protocol::Message as ProtocolMessage;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::{checkpoints, descriptor, paths::Layout, runner, sparse_copy};

#[cfg(feature = "server-ui")]
include!(concat!(env!("OUT_DIR"), "/lnx_server_ui_assets.rs"));

#[cfg(test)]
use std::io::Cursor;

#[derive(Debug)]
pub struct ServeConfig {
    pub listen: String,
    pub cpus: u8,
    pub memory_mib: u32,
    pub nested_kvm: bool,
    pub no_host_shares: bool,
}

#[derive(Debug)]
pub struct PushConfig {
    pub source: Layout,
    pub url: String,
    pub target_instance: String,
    pub replace: bool,
    pub start: bool,
    pub idle_ttl_ms: Option<u64>,
    pub command: Vec<String>,
}

#[derive(Clone)]
struct AppState {
    cpus: u8,
    memory_mib: u32,
    nested_kvm: bool,
    no_host_shares: bool,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

#[derive(Serialize)]
struct ApiMessage<'a> {
    ok: bool,
    message: &'a str,
}

#[derive(Debug, Serialize)]
struct ImportResponse {
    ok: bool,
    instance: String,
    imported: String,
    started: bool,
    status: Option<i32>,
}

#[derive(Serialize)]
struct LifecycleResponse {
    ok: bool,
    instance: String,
    state: &'static str,
    message: &'static str,
}

#[derive(Serialize)]
struct InstancesResponse {
    instances: Vec<InstanceSummary>,
}

#[derive(Serialize)]
struct InstanceSummary {
    name: String,
    state: &'static str,
    pids: Vec<i32>,
    cpus: u8,
    memory_mib: u32,
    image: Option<String>,
    rootfs_size_bytes: Option<u64>,
    rootfs_allocated_bytes: Option<u64>,
    checkpoints: usize,
    has_snapshot: bool,
}

#[derive(Deserialize)]
struct TerminalResize {
    #[serde(rename = "type")]
    kind: String,
    rows: u16,
    cols: u16,
}

enum BrokerInput {
    Data(Vec<u8>),
    Resize { rows: u16, cols: u16 },
    Close,
}

enum BrokerOutput {
    Data(Vec<u8>),
    Text(String),
    Done,
}

#[derive(Debug)]
struct ImportOptions {
    source_instance: String,
    replace: bool,
    start: bool,
    idle_ttl_ms: Option<u64>,
    command: Vec<String>,
}

const CAS_BLOCK_SIZE: u64 = 64 * 1024;
const CAS_MANIFEST_MAX_BYTES: usize = 512 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
struct CasUploadManifest {
    version: u32,
    source_instance: String,
    replace: bool,
    start: bool,
    idle_ttl_ms: Option<u64>,
    command: Vec<String>,
    files: Vec<CasManifestFile>,
}

#[derive(Serialize, Deserialize)]
struct CasUploadSession {
    target_instance: String,
    manifest: CasUploadManifest,
}

#[derive(Clone, Serialize, Deserialize)]
struct CasManifestFile {
    path: String,
    len: u64,
    mode: u32,
    blocks: Vec<CasManifestBlock>,
}

#[derive(Clone, Serialize, Deserialize)]
struct CasManifestBlock {
    offset: u64,
    len: u64,
    sha256: String,
}

#[derive(Serialize, Deserialize)]
struct CasUploadStartResponse {
    ok: bool,
    session: String,
    missing: Vec<String>,
    missing_blocks: usize,
    known_blocks: usize,
    logical_bytes: u64,
    missing_bytes: u64,
}

#[derive(Serialize, Deserialize)]
struct CasBlocksResponse {
    ok: bool,
    blocks: usize,
}

#[derive(Serialize, Deserialize)]
struct SparseBundleManifest {
    version: u32,
    source_instance: String,
    #[serde(default)]
    compression: Option<String>,
    files: Vec<SparseBundleFile>,
}

#[derive(Serialize, Deserialize)]
struct SparseBundleFile {
    path: String,
    len: u64,
    mode: u32,
    extents: Vec<SparseExtent>,
}

#[derive(Clone, Serialize, Deserialize)]
struct SparseExtent {
    offset: u64,
    len: u64,
    #[serde(default)]
    compressed_len: Option<u64>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ApiMessage {
                ok: false,
                message: &self.message,
            }),
        )
            .into_response()
    }
}

impl ApiError {
    fn bad_request(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: error.to_string(),
        }
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
        }
    }
}

pub fn serve(config: ServeConfig) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(serve_async(config))
}

async fn serve_async(config: ServeConfig) -> Result<()> {
    let addr: SocketAddr = config
        .listen
        .parse()
        .with_context(|| format!("parse listen address {}", config.listen))?;
    let state = Arc::new(AppState {
        cpus: config.cpus,
        memory_mib: config.memory_mib,
        nested_kvm: config.nested_kvm,
        no_host_shares: config.no_host_shares,
    });
    let app = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/instances", get(list_instances))
        .route("/v1/instances/{instance}/start", post(start_instance))
        .route("/v1/instances/{instance}/stop", post(stop_instance))
        .route("/v1/instances/{instance}/terminal", get(terminal_ws))
        .route("/v1/sandboxes/{instance}", put(import_sandbox))
        .route("/v2/sandboxes/{instance}/uploads", post(start_cas_upload))
        .route("/v2/uploads/{session}/blocks", put(upload_cas_blocks))
        .route("/v2/uploads/{session}/commit", post(commit_cas_upload))
        .route("/v2/uploads/{session}", delete(abort_cas_upload))
        .with_state(state);
    #[cfg(feature = "server-ui")]
    let app = app
        .route("/", get(ui_index))
        .route("/assets/{*path}", get(ui_asset));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("listen on {addr}"))?;
    println!("lnx server listening on http://{addr}");
    axum::serve(listener, app).await.context("serve lnx API")
}

async fn health() -> Json<ApiMessage<'static>> {
    Json(ApiMessage {
        ok: true,
        message: "ok",
    })
}

#[cfg(feature = "server-ui")]
async fn ui_index() -> std::result::Result<Response, ApiError> {
    serve_ui_asset("/index.html")
}

#[cfg(feature = "server-ui")]
async fn ui_asset(AxumPath(path): AxumPath<String>) -> std::result::Result<Response, ApiError> {
    serve_ui_asset(&format!("/assets/{path}"))
}

#[cfg(feature = "server-ui")]
fn serve_ui_asset(path: &str) -> std::result::Result<Response, ApiError> {
    let asset = SERVER_UI_ASSETS
        .iter()
        .find(|asset| asset.path == path)
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            message: "asset not found".to_string(),
        })?;
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, asset.mime)
        .body(Body::from(asset.bytes))
        .map_err(ApiError::internal)
}

async fn list_instances(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<InstancesResponse>, ApiError> {
    let result = tokio::task::spawn_blocking(move || instance_summaries(&state))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
    Ok(Json(InstancesResponse { instances: result }))
}

async fn terminal_ws(
    ws: WebSocketUpgrade,
    AxumPath(instance): AxumPath<String>,
) -> std::result::Result<Response, ApiError> {
    validate_instance_name(&instance).map_err(ApiError::bad_request)?;
    Ok(ws
        .on_upgrade(move |socket| terminal_session(socket, instance))
        .into_response())
}

async fn start_instance(
    State(state): State<Arc<AppState>>,
    AxumPath(instance): AxumPath<String>,
) -> std::result::Result<Json<LifecycleResponse>, ApiError> {
    validate_instance_name(&instance).map_err(ApiError::bad_request)?;
    let start_state = (*state).clone();
    let instance_name = instance.clone();
    let response = tokio::task::spawn_blocking(move || {
        let layout = Layout::resolve(&instance_name, None, None)?;
        start_existing_instance(&layout, &start_state)?;
        Ok::<_, anyhow::Error>(LifecycleResponse {
            ok: true,
            instance: instance_name,
            state: instance_state(&layout),
            message: "started",
        })
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::bad_request)?;
    Ok(Json(response))
}

async fn stop_instance(
    AxumPath(instance): AxumPath<String>,
) -> std::result::Result<Json<LifecycleResponse>, ApiError> {
    validate_instance_name(&instance).map_err(ApiError::bad_request)?;
    let layout = Layout::resolve(&instance, None, None).map_err(ApiError::bad_request)?;
    stop_existing_instance(&layout)
        .await
        .map_err(ApiError::bad_request)?;
    let response = LifecycleResponse {
        ok: true,
        instance,
        state: instance_state(&layout),
        message: "stopped",
    };
    Ok(Json(response))
}

async fn terminal_session(socket: WebSocket, instance: String) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (broker_tx, mut broker_rx) = tokio::sync::mpsc::channel::<BrokerOutput>(128);
    let (input_tx, input_rx) = std::sync::mpsc::channel::<BrokerInput>();
    let setup = setup_terminal_broker(&instance, broker_tx.clone(), input_rx);
    let Err(error) = setup else {
        loop {
            tokio::select! {
                Some(output) = broker_rx.recv() => {
                    match output {
                        BrokerOutput::Data(bytes) => {
                            if ws_tx.send(WsMessage::Binary(bytes.into())).await.is_err() {
                                break;
                            }
                        }
                        BrokerOutput::Text(text) => {
                            if ws_tx.send(WsMessage::Text(text.into())).await.is_err() {
                                break;
                            }
                        }
                        BrokerOutput::Done => break,
                    }
                }
                Some(message) = ws_rx.next() => {
                    let Ok(message) = message else {
                        break;
                    };
                    if handle_terminal_ws_message(message, &input_tx).is_err() {
                        break;
                    }
                }
                else => break,
            }
        }
        let _ = input_tx.send(BrokerInput::Close);
        return;
    };
    let _ = ws_tx
        .send(WsMessage::Text(
            format!("[terminal failed: {error:#}]\r\n").into(),
        ))
        .await;
}

fn handle_terminal_ws_message(
    message: WsMessage,
    input_tx: &std::sync::mpsc::Sender<BrokerInput>,
) -> Result<()> {
    match message {
        WsMessage::Text(text) => {
            let text = text.to_string();
            if text.starts_with("{\"type\"") {
                if let Ok(resize) = serde_json::from_str::<TerminalResize>(&text) {
                    if resize.kind == "resize" {
                        input_tx
                            .send(BrokerInput::Resize {
                                rows: resize.rows,
                                cols: resize.cols,
                            })
                            .context("send resize to broker")?;
                        return Ok(());
                    }
                }
            }
            input_tx
                .send(BrokerInput::Data(text.into_bytes()))
                .context("send data to broker")?;
        }
        WsMessage::Binary(bytes) => {
            input_tx
                .send(BrokerInput::Data(bytes.to_vec()))
                .context("send binary data to broker")?;
        }
        WsMessage::Close(_) => {
            input_tx
                .send(BrokerInput::Close)
                .context("send close to broker")?;
        }
        WsMessage::Ping(_) | WsMessage::Pong(_) => {}
    }
    Ok(())
}

fn setup_terminal_broker(
    instance: &str,
    output_tx: tokio::sync::mpsc::Sender<BrokerOutput>,
    input_rx: std::sync::mpsc::Receiver<BrokerInput>,
) -> Result<()> {
    let layout = Layout::resolve(instance, None, None)?;
    let broker = layout.run_dir.join("broker.sock");
    let mut stream = runner::connect_broker(&broker)
        .with_context(|| format!("connect running instance {instance}"))?;
    let channel_id = runner::new_request_id()?;
    runner::write_message(
        &mut stream,
        &ProtocolMessage::OpenExec {
            channel_id,
            argv: Vec::new(),
            cwd: "/".to_string(),
            pty: true,
            term: "xterm-256color".to_string(),
            colorterm: "truecolor".to_string(),
            rows: 24,
            cols: 80,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            group: host_group_name(),
            env: terminal_env(),
        },
    )?;

    let mut input_stream = stream
        .try_clone()
        .context("clone broker stream for terminal input")?;
    std::thread::spawn(move || {
        for input in input_rx {
            let message = match input {
                BrokerInput::Data(bytes) => ProtocolMessage::Data { channel_id, bytes },
                BrokerInput::Resize { rows, cols } => ProtocolMessage::WindowResize {
                    channel_id,
                    rows,
                    cols,
                },
                BrokerInput::Close => {
                    let _ = runner::write_message(
                        &mut input_stream,
                        &ProtocolMessage::Eof { channel_id },
                    );
                    let _ = runner::write_message(
                        &mut input_stream,
                        &ProtocolMessage::Close { channel_id },
                    );
                    break;
                }
            };
            if runner::write_message(&mut input_stream, &message).is_err() {
                break;
            }
        }
    });

    std::thread::spawn(move || {
        loop {
            match runner::read_message(&mut stream) {
                Ok(ProtocolMessage::Data {
                    channel_id: id,
                    bytes,
                })
                | Ok(ProtocolMessage::Stderr {
                    channel_id: id,
                    bytes,
                }) if id == channel_id => {
                    if output_tx.blocking_send(BrokerOutput::Data(bytes)).is_err() {
                        break;
                    }
                }
                Ok(ProtocolMessage::ExitStatus {
                    channel_id: id,
                    status,
                }) if id == channel_id => {
                    let _ = output_tx.blocking_send(BrokerOutput::Text(format!(
                        "\r\n[process exited: {status}]\r\n"
                    )));
                    let _ = output_tx.blocking_send(BrokerOutput::Done);
                    break;
                }
                Ok(ProtocolMessage::Error {
                    channel_id: id,
                    message,
                }) if id == channel_id => {
                    let _ = output_tx
                        .blocking_send(BrokerOutput::Text(format!("\r\n[error: {message}]\r\n")));
                    let _ = output_tx.blocking_send(BrokerOutput::Done);
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = output_tx.blocking_send(BrokerOutput::Text(format!(
                        "\r\n[broker error: {error:#}]\r\n"
                    )));
                    let _ = output_tx.blocking_send(BrokerOutput::Done);
                    break;
                }
            }
        }
    });
    Ok(())
}

fn terminal_env() -> Vec<(String, String)> {
    let mut env = vec![
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("COLORTERM".to_string(), "truecolor".to_string()),
    ];
    for key in [
        "LANG",
        "LANGUAGE",
        "TZ",
        "NO_COLOR",
        "CLICOLOR",
        "CLICOLOR_FORCE",
    ] {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() {
                env.push((key.to_string(), value));
            }
        }
    }
    for (key, value) in std::env::vars() {
        if key.starts_with("LC_") && !value.is_empty() && !env.iter().any(|(k, _)| k == &key) {
            env.push((key, value));
        }
    }
    env
}

fn host_group_name() -> String {
    let gid = unsafe { libc::getgid() };
    let mut buf = [0u8; 1024];
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::group = std::ptr::null_mut();
    let rc = unsafe {
        libc::getgrgid_r(
            gid,
            &mut grp,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() {
        return String::new();
    }
    unsafe { std::ffi::CStr::from_ptr(grp.gr_name) }
        .to_string_lossy()
        .into_owned()
}

fn instance_summaries(state: &AppState) -> Result<Vec<InstanceSummary>> {
    let default_layout = Layout::resolve("default", None, None)?;
    let mut names = BTreeSet::new();
    collect_child_dir_names(&default_layout.base.join("instances"), &mut names)?;
    let mut instances = names
        .into_iter()
        .map(|name| {
            let layout = Layout::resolve(&name, None, None)?;
            let descriptor = descriptor::load(&layout)?;
            let checkpoints = match fs::read_dir(&layout.checkpoint_dir) {
                Ok(entries) => entries
                    .filter_map(|entry| entry.ok())
                    .filter(|entry| entry.file_type().is_ok_and(|file_type| file_type.is_dir()))
                    .count(),
                Err(_) => 0,
            };
            Ok(InstanceSummary {
                name,
                state: instance_state(&layout),
                pids: instance_pids(&layout),
                cpus: descriptor.cpus.unwrap_or(state.cpus),
                memory_mib: descriptor.memory_mib.unwrap_or(state.memory_mib),
                image: descriptor.image,
                rootfs_size_bytes: file_len(&layout.rootfs),
                rootfs_allocated_bytes: allocated_bytes(&layout.rootfs),
                checkpoints,
                has_snapshot: layout.snapshot_dir.join("latest").exists(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    instances.sort_by_key(|instance| {
        (
            instance_state_rank(instance.state),
            instance.name.to_ascii_lowercase(),
        )
    });
    Ok(instances)
}

fn collect_child_dir_names(parent: &Path, names: &mut BTreeSet<String>) -> Result<()> {
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("read {}", parent.display())),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() && !crate::paths::is_instance_transaction_root(&entry.path())
        {
            names.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(())
}

fn instance_state(layout: &Layout) -> &'static str {
    let broker = layout.run_dir.join("broker.sock");
    if broker.exists() && runner::connect_broker(&broker).is_ok() {
        "running"
    } else if alive_owner_pid(&layout.run_dir.join("bootstrap.lock.d")).is_some() {
        "starting"
    } else if layout.rootfs.exists() {
        "stopped"
    } else {
        "partial"
    }
}

fn instance_state_rank(state: &str) -> u8 {
    match state {
        "running" => 0,
        "starting" => 1,
        "stopped" => 2,
        _ => 3,
    }
}

fn instance_pids(layout: &Layout) -> Vec<i32> {
    let mut pids = BTreeMap::new();
    if let Some(pid) = alive_owner_pid(&layout.run_dir.join("bootstrap.lock.d")) {
        pids.insert(pid, ());
    }
    for pid in host_pids_for_instance(&layout.instance) {
        pids.insert(pid, ());
    }
    pids.keys().copied().collect()
}

fn alive_owner_pid(lock_dir: &Path) -> Option<i32> {
    let pid = recorded_owner_pid(lock_dir)?;
    process_alive(pid).then_some(pid)
}

fn recorded_owner_pid(lock_dir: &Path) -> Option<i32> {
    fs::read_to_string(lock_dir.join("owner.pid"))
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()
}

fn host_pids_for_instance(instance: &str) -> Vec<i32> {
    let output = Command::new("pgrep")
        .arg("-f")
        .arg(format!("--instance[= ]{instance}"))
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .filter(|pid| *pid != std::process::id() as i32 && process_alive(*pid))
        .collect()
}

fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe {
        libc::kill(pid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

fn file_len(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|meta| meta.len())
}

fn allocated_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).ok().map(|meta| meta.blocks() * 512)
}

async fn import_sandbox(
    State(state): State<Arc<AppState>>,
    AxumPath(instance): AxumPath<String>,
    headers: HeaderMap,
    body: Body,
) -> std::result::Result<Json<ImportResponse>, ApiError> {
    validate_instance_name(&instance).map_err(ApiError::bad_request)?;
    let options = import_options(&headers).map_err(ApiError::bad_request)?;
    validate_instance_name(&options.source_instance).map_err(ApiError::bad_request)?;
    let content_type = header_string(&headers, "content-type")
        .map_err(ApiError::bad_request)?
        .unwrap_or_default();
    let bundle = tempfile::NamedTempFile::new().map_err(ApiError::internal)?;
    let bundle_path = bundle.path().to_path_buf();
    write_body_to_file(body, &bundle_path).await?;

    let import_state = (*state).clone();
    let target = instance.clone();
    let result = tokio::task::spawn_blocking(move || {
        if content_type
            .split(';')
            .next()
            .is_some_and(|value| value.trim() == "application/x-lnx-sparse-bundle")
        {
            import_sparse_bundle(&bundle_path, &target, options, import_state)
        } else {
            import_archive(&bundle_path, &target, options, import_state)
        }
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)?;

    Ok(Json(result))
}

async fn start_cas_upload(
    State(state): State<Arc<AppState>>,
    AxumPath(instance): AxumPath<String>,
    body: Body,
) -> std::result::Result<Json<CasUploadStartResponse>, ApiError> {
    validate_instance_name(&instance).map_err(ApiError::bad_request)?;
    let bytes = to_bytes(body, CAS_MANIFEST_MAX_BYTES)
        .await
        .map_err(ApiError::bad_request)?;
    let manifest: CasUploadManifest =
        serde_json::from_slice(&bytes).map_err(ApiError::bad_request)?;
    validate_instance_name(&manifest.source_instance).map_err(ApiError::bad_request)?;
    let upload_state = (*state).clone();
    let target = instance.clone();
    let response = tokio::task::spawn_blocking(move || {
        start_cas_upload_blocking(&target, manifest, upload_state)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::bad_request)?;
    Ok(Json(response))
}

async fn upload_cas_blocks(
    AxumPath(session): AxumPath<String>,
    headers: HeaderMap,
    body: Body,
) -> std::result::Result<Json<CasBlocksResponse>, ApiError> {
    validate_session_id(&session).map_err(ApiError::bad_request)?;
    let compressed = header_string(&headers, "content-encoding")
        .map_err(ApiError::bad_request)?
        .is_some_and(|value| value.eq_ignore_ascii_case("zstd"));
    let upload = tempfile::Builder::new()
        .prefix("lnx-cas-blocks-")
        .tempfile()
        .map_err(ApiError::internal)?;
    let upload_path = upload.path().to_path_buf();
    write_body_to_file(body, &upload_path).await?;
    let response = tokio::task::spawn_blocking(move || {
        let _upload = upload;
        let file = fs::File::open(&upload_path).context("open CAS block stream upload")?;
        let mut input: Box<dyn Read> = if compressed {
            Box::new(zstd::stream::read::Decoder::new(file).context("decompress CAS block stream")?)
        } else {
            Box::new(file)
        };
        let blocks = store_cas_block_stream(&mut input)?;
        Ok::<_, anyhow::Error>(CasBlocksResponse { ok: true, blocks })
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::bad_request)?;
    Ok(Json(response))
}

async fn commit_cas_upload(
    State(state): State<Arc<AppState>>,
    AxumPath(session): AxumPath<String>,
) -> std::result::Result<Json<ImportResponse>, ApiError> {
    validate_session_id(&session).map_err(ApiError::bad_request)?;
    let commit_state = (*state).clone();
    let response =
        tokio::task::spawn_blocking(move || commit_cas_upload_blocking(&session, commit_state))
            .await
            .map_err(ApiError::internal)?
            .map_err(ApiError::bad_request)?;
    Ok(Json(response))
}

async fn abort_cas_upload(
    AxumPath(session): AxumPath<String>,
) -> std::result::Result<Json<ApiMessage<'static>>, ApiError> {
    validate_session_id(&session).map_err(ApiError::bad_request)?;
    tokio::task::spawn_blocking(move || remove_path_if_exists(&cas_session_dir(&session)?))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
    Ok(Json(ApiMessage {
        ok: true,
        message: "aborted",
    }))
}

async fn write_body_to_file(body: Body, path: &Path) -> std::result::Result<(), ApiError> {
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(ApiError::internal)?;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(ApiError::bad_request)?;
        file.write_all(&chunk).await.map_err(ApiError::internal)?;
    }
    file.flush().await.map_err(ApiError::internal)
}

fn import_options(headers: &HeaderMap) -> Result<ImportOptions> {
    let source_instance = header_string(headers, "x-lnx-source-instance")?
        .context("missing x-lnx-source-instance header")?;
    let command = match header_string(headers, "x-lnx-command-json")? {
        Some(value) if !value.is_empty() => serde_json::from_str(&value)
            .with_context(|| format!("parse x-lnx-command-json: {value}"))?,
        _ => Vec::new(),
    };
    Ok(ImportOptions {
        source_instance,
        replace: header_bool(headers, "x-lnx-replace")?,
        start: header_bool(headers, "x-lnx-start")?,
        idle_ttl_ms: header_string(headers, "x-lnx-idle-ttl-ms")?
            .filter(|value| !value.is_empty())
            .map(|value| value.parse().context("parse x-lnx-idle-ttl-ms"))
            .transpose()?,
        command,
    })
}

fn header_string(headers: &HeaderMap, name: &str) -> Result<Option<String>> {
    headers
        .get(name)
        .map(|value| {
            value
                .to_str()
                .with_context(|| format!("{name} is not valid UTF-8"))
                .map(ToOwned::to_owned)
        })
        .transpose()
}

fn header_bool(headers: &HeaderMap, name: &str) -> Result<bool> {
    Ok(matches!(
        header_string(headers, name)?.as_deref(),
        Some("1" | "true" | "yes")
    ))
}

fn import_archive(
    archive: &Path,
    target_instance: &str,
    options: ImportOptions,
    state: AppState,
) -> Result<ImportResponse> {
    let dest = Layout::resolve(target_instance, None, None)?;
    import_archive_to_layout(archive, &dest, target_instance, options, state)
}

fn import_sparse_bundle(
    bundle: &Path,
    target_instance: &str,
    options: ImportOptions,
    state: AppState,
) -> Result<ImportResponse> {
    let dest = Layout::resolve(target_instance, None, None)?;
    import_sparse_bundle_to_layout(bundle, &dest, target_instance, options, state)
}

fn import_sparse_bundle_to_layout(
    bundle: &Path,
    dest: &Layout,
    target_instance: &str,
    options: ImportOptions,
    state: AppState,
) -> Result<ImportResponse> {
    reject_running_instance(dest)?;
    if (dest.rootfs.exists() || dest.snapshot_dir.exists()) && !options.replace {
        bail!("target instance already exists: {target_instance} (use --replace)");
    }

    fs::create_dir_all(&dest.base).with_context(|| format!("create {}", dest.base.display()))?;
    let temp = tempfile::Builder::new()
        .prefix(".import-")
        .tempdir_in(&dest.base)
        .with_context(|| format!("create import tempdir in {}", dest.base.display()))?;
    extract_sparse_bundle(bundle, temp.path(), &options.source_instance)?;
    let imported = temp.path().join("instances").join(&options.source_instance);
    if !imported.join("rootfs.ext4").exists() {
        bail!(
            "sandbox bundle is missing instances/{}/rootfs.ext4",
            options.source_instance
        );
    }
    validate_imported_snapshot(&imported, &state)?;

    if options.replace {
        remove_path_if_exists(&dest.instance_dir)?;
        if dest.run_dir != dest.instance_dir {
            remove_path_if_exists(&dest.run_dir)?;
        }
    }
    fs::create_dir_all(
        dest.instance_dir
            .parent()
            .context("instance dir has no parent")?,
    )
    .with_context(|| format!("create {}", dest.instance_dir.parent().unwrap().display()))?;
    fs::rename(&imported, &dest.instance_dir).with_context(|| {
        format!(
            "move imported sandbox {} to {}",
            imported.display(),
            dest.instance_dir.display()
        )
    })?;
    install_kernel_if_present(temp.path(), dest)?;
    rewrite_descriptor_name(dest)?;
    index_layout_into_cas(dest)?;

    let status = if options.start {
        Some(start_imported_instance(dest, &options, &state)?)
    } else {
        None
    };
    Ok(ImportResponse {
        ok: true,
        instance: target_instance.to_string(),
        imported: options.source_instance,
        started: options.start,
        status,
    })
}

fn import_archive_to_layout(
    archive: &Path,
    dest: &Layout,
    target_instance: &str,
    options: ImportOptions,
    state: AppState,
) -> Result<ImportResponse> {
    reject_running_instance(dest)?;
    if (dest.rootfs.exists() || dest.snapshot_dir.exists()) && !options.replace {
        bail!("target instance already exists: {target_instance} (use --replace)");
    }

    fs::create_dir_all(&dest.base).with_context(|| format!("create {}", dest.base.display()))?;
    let temp = tempfile::Builder::new()
        .prefix(".import-")
        .tempdir_in(&dest.base)
        .with_context(|| format!("create import tempdir in {}", dest.base.display()))?;
    extract_archive(archive, temp.path())?;
    let imported = temp.path().join("instances").join(&options.source_instance);
    if !imported.join("rootfs.ext4").exists() {
        bail!(
            "sandbox bundle is missing instances/{}/rootfs.ext4",
            options.source_instance
        );
    }
    validate_imported_snapshot(&imported, &state)?;

    if options.replace {
        remove_path_if_exists(&dest.instance_dir)?;
        if dest.run_dir != dest.instance_dir {
            remove_path_if_exists(&dest.run_dir)?;
        }
    }
    fs::create_dir_all(
        dest.instance_dir
            .parent()
            .context("instance dir has no parent")?,
    )
    .with_context(|| format!("create {}", dest.instance_dir.parent().unwrap().display()))?;
    fs::rename(&imported, &dest.instance_dir).with_context(|| {
        format!(
            "move imported sandbox {} to {}",
            imported.display(),
            dest.instance_dir.display()
        )
    })?;
    install_kernel_if_present(temp.path(), dest)?;
    rewrite_descriptor_name(dest)?;
    index_layout_into_cas(dest)?;

    let status = if options.start {
        Some(start_imported_instance(dest, &options, &state)?)
    } else {
        None
    };
    Ok(ImportResponse {
        ok: true,
        instance: target_instance.to_string(),
        imported: options.source_instance,
        started: options.start,
        status,
    })
}

fn extract_archive(archive: &Path, dest: &Path) -> Result<()> {
    let status = Command::new("tar")
        .arg("-C")
        .arg(dest)
        .arg("-xf")
        .arg(archive)
        .status()
        .context("extract sandbox bundle with tar")?;
    if !status.success() {
        bail!("tar extract failed with status {status}");
    }
    Ok(())
}

const SPARSE_BUNDLE_MAGIC: &[u8; 8] = b"LNXSBX1\n";

fn extract_sparse_bundle(bundle: &Path, dest: &Path, expected_source_instance: &str) -> Result<()> {
    let mut input = fs::File::open(bundle).with_context(|| format!("open {}", bundle.display()))?;
    let mut magic = [0u8; 8];
    input
        .read_exact(&mut magic)
        .context("read sparse bundle magic")?;
    if &magic != SPARSE_BUNDLE_MAGIC {
        bail!("invalid sparse bundle magic");
    }
    let manifest_len = read_u64(&mut input).context("read sparse bundle manifest length")?;
    if manifest_len > 16 * 1024 * 1024 {
        bail!("sparse bundle manifest is too large: {manifest_len} bytes");
    }
    let mut manifest_json = vec![0u8; manifest_len as usize];
    input
        .read_exact(&mut manifest_json)
        .context("read sparse bundle manifest")?;
    let manifest: SparseBundleManifest =
        serde_json::from_slice(&manifest_json).context("parse sparse bundle manifest")?;
    if manifest.version != 1 {
        bail!("unsupported sparse bundle version: {}", manifest.version);
    }
    if !matches!(manifest.compression.as_deref(), None | Some("zstd")) {
        bail!(
            "unsupported sparse bundle compression: {}",
            manifest.compression.unwrap_or_default()
        );
    }
    if manifest.source_instance != expected_source_instance {
        bail!(
            "sparse bundle source instance mismatch: header={}, bundle={}",
            expected_source_instance,
            manifest.source_instance
        );
    }

    for file in manifest.files {
        let relative = safe_bundle_relative_path(&file.path)?;
        let out_path = dest.join(relative);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let mut output = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&out_path)
            .with_context(|| format!("create {}", out_path.display()))?;
        output
            .set_len(file.len)
            .with_context(|| format!("truncate {}", out_path.display()))?;
        for extent in file.extents {
            if extent.offset > file.len || extent.len > file.len.saturating_sub(extent.offset) {
                bail!("extent out of bounds for {}", file.path);
            }
            output
                .seek(SeekFrom::Start(extent.offset))
                .with_context(|| format!("seek {}", out_path.display()))?;
            if let Some(compressed_len) = extent.compressed_len {
                let mut compressed = std::io::Read::by_ref(&mut input).take(compressed_len);
                let mut counted = CountingWriter::new(&mut output);
                zstd::stream::copy_decode(&mut compressed, &mut counted).with_context(|| {
                    format!(
                        "decompress extent {} at {}",
                        out_path.display(),
                        extent.offset
                    )
                })?;
                if counted.written != extent.len {
                    bail!(
                        "decompressed extent length mismatch for {} at {}: expected {}, got {}",
                        out_path.display(),
                        extent.offset,
                        extent.len,
                        counted.written
                    );
                }
            } else {
                copy_exact_bytes(&mut input, &mut output, extent.len)
                    .with_context(|| format!("write extent {}", out_path.display()))?;
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&out_path, fs::Permissions::from_mode(file.mode & 0o7777))
                .with_context(|| format!("chmod {}", out_path.display()))?;
        }
    }
    Ok(())
}

fn read_u64(input: &mut impl Read) -> std::io::Result<u64> {
    let mut bytes = [0u8; 8];
    input.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn copy_exact_bytes(input: &mut impl Read, output: &mut impl Write, mut len: u64) -> Result<()> {
    let mut buf = vec![0u8; 1024 * 1024];
    while len > 0 {
        let want = len.min(buf.len() as u64) as usize;
        input.read_exact(&mut buf[..want])?;
        output.write_all(&buf[..want])?;
        len -= want as u64;
    }
    Ok(())
}

struct CountingWriter<W> {
    inner: W,
    written: u64,
}

impl<W> CountingWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner, written: 0 }
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn safe_bundle_relative_path(path: &str) -> Result<&Path> {
    let relative = Path::new(path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        bail!("unsafe sparse bundle path: {path}");
    }
    Ok(relative)
}

fn install_kernel_if_present(bundle: &Path, dest: &Layout) -> Result<()> {
    let kernel = bundle.join("vmlinuz");
    if kernel.exists() && !dest.kernel.exists() {
        sparse_copy::clone_or_copy_file(&kernel, &dest.kernel)?;
    }
    Ok(())
}

fn rewrite_descriptor_name(layout: &Layout) -> Result<()> {
    let mut config = descriptor::load(layout)?;
    config.name = Some(layout.instance.clone());
    descriptor::save(layout, &config)
}

fn start_cas_upload_blocking(
    target_instance: &str,
    manifest: CasUploadManifest,
    _state: AppState,
) -> Result<CasUploadStartResponse> {
    if manifest.version != 2 {
        bail!(
            "unsupported CAS upload manifest version: {}",
            manifest.version
        );
    }
    let dest = Layout::resolve(target_instance, None, None)?;
    reject_running_instance(&dest)?;
    if (dest.rootfs.exists() || dest.snapshot_dir.exists()) && !manifest.replace {
        bail!("target instance already exists: {target_instance} (use --replace)");
    }
    validate_cas_manifest(&manifest)?;

    fs::create_dir_all(cas_root()?).context("create CAS root")?;
    fs::create_dir_all(cas_uploads_root()?).context("create CAS uploads root")?;
    let session = new_upload_session_id(target_instance)?;
    let session_dir = cas_session_dir(&session)?;
    fs::create_dir_all(&session_dir)
        .with_context(|| format!("create {}", session_dir.display()))?;
    let session_doc = CasUploadSession {
        target_instance: target_instance.to_string(),
        manifest: manifest.clone(),
    };
    let session_json = serde_json::to_vec_pretty(&session_doc).context("encode upload session")?;
    fs::write(session_dir.join("session.json"), session_json)
        .with_context(|| format!("write {}", session_dir.join("session.json").display()))?;

    let mut missing = Vec::new();
    let mut seen = BTreeSet::new();
    let mut known_blocks = 0usize;
    let mut missing_bytes = 0u64;
    let mut logical_bytes = 0u64;
    let zero_hash = zero_block_hash();
    for file in &manifest.files {
        logical_bytes = logical_bytes.saturating_add(file.len);
        for block in &file.blocks {
            if !seen.insert(block.sha256.clone()) {
                continue;
            }
            if block.sha256 == zero_hash || cas_block_path(&block.sha256)?.exists() {
                known_blocks += 1;
            } else {
                missing_bytes = missing_bytes.saturating_add(block.len);
                missing.push(block.sha256.clone());
            }
        }
    }
    Ok(CasUploadStartResponse {
        ok: true,
        session,
        missing_blocks: missing.len(),
        known_blocks,
        logical_bytes,
        missing_bytes,
        missing,
    })
}

fn commit_cas_upload_blocking(session: &str, state: AppState) -> Result<ImportResponse> {
    let session_dir = cas_session_dir(session)?;
    let session_json = fs::read_to_string(session_dir.join("session.json"))
        .with_context(|| format!("read {}", session_dir.join("session.json").display()))?;
    let session_doc: CasUploadSession =
        serde_json::from_str(&session_json).context("parse upload session")?;
    let manifest = session_doc.manifest;
    let target_instance = session_doc.target_instance;
    let dest = Layout::resolve(&target_instance, None, None)?;
    reject_running_instance(&dest)?;
    if (dest.rootfs.exists() || dest.snapshot_dir.exists()) && !manifest.replace {
        bail!("target instance already exists: {target_instance} (use --replace)");
    }
    validate_cas_manifest(&manifest)?;
    validate_imported_snapshot_manifest(&manifest, &state)?;
    verify_cas_manifest_blocks_exist(&manifest)?;

    fs::create_dir_all(&dest.base).with_context(|| format!("create {}", dest.base.display()))?;
    let temp = tempfile::Builder::new()
        .prefix(".import-")
        .tempdir_in(&dest.base)
        .with_context(|| format!("create import tempdir in {}", dest.base.display()))?;
    reconstruct_cas_manifest(&manifest, temp.path())?;
    let imported = temp
        .path()
        .join("instances")
        .join(&manifest.source_instance);
    if !imported.join("rootfs.ext4").exists() {
        bail!(
            "sandbox bundle is missing instances/{}/rootfs.ext4",
            manifest.source_instance
        );
    }
    validate_imported_snapshot(&imported, &state)?;

    if manifest.replace {
        remove_path_if_exists(&dest.instance_dir)?;
        if dest.run_dir != dest.instance_dir {
            remove_path_if_exists(&dest.run_dir)?;
        }
    }
    fs::create_dir_all(
        dest.instance_dir
            .parent()
            .context("instance dir has no parent")?,
    )
    .with_context(|| format!("create {}", dest.instance_dir.parent().unwrap().display()))?;
    fs::rename(&imported, &dest.instance_dir).with_context(|| {
        format!(
            "move imported sandbox {} to {}",
            imported.display(),
            dest.instance_dir.display()
        )
    })?;
    install_kernel_if_present(temp.path(), &dest)?;
    rewrite_descriptor_name(&dest)?;
    index_layout_into_cas(&dest)?;
    remove_path_if_exists(&session_dir)?;

    let status = if manifest.start {
        let options = ImportOptions {
            source_instance: manifest.source_instance.clone(),
            replace: manifest.replace,
            start: manifest.start,
            idle_ttl_ms: manifest.idle_ttl_ms,
            command: manifest.command.clone(),
        };
        Some(start_imported_instance(&dest, &options, &state)?)
    } else {
        None
    };
    Ok(ImportResponse {
        ok: true,
        instance: target_instance,
        imported: manifest.source_instance,
        started: manifest.start,
        status,
    })
}

fn validate_imported_snapshot_manifest(
    manifest: &CasUploadManifest,
    state: &AppState,
) -> Result<()> {
    let has_snapshot = manifest.files.iter().any(|file| {
        file.path
            == format!(
                "instances/{}/memory-snapshots/latest/vmstate.bin",
                manifest.source_instance
            )
    });
    if !has_snapshot {
        return Ok(());
    }
    let shares_path = format!(
        "instances/{}/memory-snapshots/latest/launch.json",
        manifest.source_instance
    );
    let Some(shares_file) = manifest.files.iter().find(|file| file.path == shares_path) else {
        bail!(
            "snapshot cannot be restored on this server because its launch metadata is missing; push a fresh checkpoint created for this server or remove the snapshot before pushing ({shares_path})"
        );
    };
    let temp = tempfile::Builder::new()
        .prefix("lnx-cas-launch-")
        .tempdir()
        .context("create launch metadata tempdir")?;
    let stamp = temp.path().join("launch.json");
    reconstruct_cas_file(shares_file, &stamp)?;
    let cwd = std::env::current_dir().context("current directory")?;
    if let Some(reason) =
        runner::snapshot_shares_incompatibility_for_import(temp.path(), &cwd, state.no_host_shares)?
    {
        bail!(
            "snapshot cannot be restored on this server because its host-share/network settings differ ({reason}); push a fresh checkpoint created for this server or remove the snapshot before pushing ({shares_path})"
        );
    }
    Ok(())
}

fn validate_cas_manifest(manifest: &CasUploadManifest) -> Result<()> {
    validate_instance_name(&manifest.source_instance)?;
    let mut paths = BTreeSet::new();
    for file in &manifest.files {
        let relative = safe_bundle_relative_path(&file.path)?;
        if !paths.insert(file.path.clone()) {
            bail!("duplicate file in CAS manifest: {}", file.path);
        }
        if relative.as_os_str().is_empty() {
            bail!("empty file path in CAS manifest");
        }
        let mut previous_end = 0u64;
        for block in &file.blocks {
            validate_sha256_hex(&block.sha256)?;
            if block.len == 0 || block.len > CAS_BLOCK_SIZE {
                bail!("invalid CAS block length {} in {}", block.len, file.path);
            }
            if block.offset > file.len || block.len > file.len.saturating_sub(block.offset) {
                bail!("CAS block out of bounds for {}", file.path);
            }
            if block.offset < previous_end {
                bail!("overlapping CAS blocks in {}", file.path);
            }
            previous_end = block.offset + block.len;
        }
    }
    Ok(())
}

fn verify_cas_manifest_blocks_exist(manifest: &CasUploadManifest) -> Result<()> {
    let zero_hash = zero_block_hash();
    for file in &manifest.files {
        for block in &file.blocks {
            if block.sha256 == zero_hash {
                continue;
            }
            if !cas_block_path(&block.sha256)?.exists() {
                bail!("missing CAS block {} for {}", block.sha256, file.path);
            }
        }
    }
    Ok(())
}

fn reconstruct_cas_manifest(manifest: &CasUploadManifest, dest: &Path) -> Result<()> {
    for file in &manifest.files {
        let relative = safe_bundle_relative_path(&file.path)?;
        let out_path = dest.join(relative);
        reconstruct_cas_file(file, &out_path)?;
    }
    Ok(())
}

fn reconstruct_cas_file(file: &CasManifestFile, out_path: &Path) -> Result<()> {
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut output = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(out_path)
        .with_context(|| format!("create {}", out_path.display()))?;
    output
        .set_len(file.len)
        .with_context(|| format!("truncate {}", out_path.display()))?;
    let zero_hash = zero_block_hash();
    for block in &file.blocks {
        output
            .seek(SeekFrom::Start(block.offset))
            .with_context(|| format!("seek {}", out_path.display()))?;
        if block.sha256 == zero_hash {
            continue;
        }
        let mut input = fs::File::open(cas_block_path(&block.sha256)?)
            .with_context(|| format!("open CAS block {}", block.sha256))?;
        copy_exact_bytes(&mut input, &mut output, block.len)
            .with_context(|| format!("write CAS block {}", out_path.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(out_path, fs::Permissions::from_mode(file.mode & 0o7777))
            .with_context(|| format!("chmod {}", out_path.display()))?;
    }
    Ok(())
}

fn index_layout_into_cas(layout: &Layout) -> Result<()> {
    for file in collect_sparse_bundle_files(layout)? {
        let mut source = fs::File::open(&file.source)
            .with_context(|| format!("open {}", file.source.display()))?;
        let mut buf = vec![0u8; CAS_BLOCK_SIZE as usize];
        for extent in file.extents {
            let mut remaining = extent.len;
            let mut offset = extent.offset;
            while remaining > 0 {
                let len = remaining.min(CAS_BLOCK_SIZE) as usize;
                source
                    .seek(SeekFrom::Start(offset))
                    .with_context(|| format!("seek {}", file.source.display()))?;
                source
                    .read_exact(&mut buf[..len])
                    .with_context(|| format!("read {}", file.source.display()))?;
                if !buf[..len].iter().all(|byte| *byte == 0) {
                    let hash = sha256_hex(&buf[..len]);
                    store_cas_block(&hash, &buf[..len])?;
                }
                remaining -= len as u64;
                offset += len as u64;
            }
        }
    }
    Ok(())
}

fn validate_imported_snapshot(imported: &Path, state: &AppState) -> Result<()> {
    let latest_snapshot = imported.join("memory-snapshots/latest");
    if !latest_snapshot.exists() {
        return Ok(());
    }
    let source_instance = imported
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown");
    let logical_stamp = format!("instances/{source_instance}/memory-snapshots/latest/launch.json");
    let cwd = std::env::current_dir().context("current directory")?;
    if let Some(reason) = runner::snapshot_shares_incompatibility_for_import(
        &latest_snapshot,
        &cwd,
        state.no_host_shares,
    )? {
        bail!(
            "snapshot cannot be restored on this server because its host-share/network settings differ ({reason}); push a fresh checkpoint created for this server or remove the snapshot before pushing ({logical_stamp})"
        );
    }
    Ok(())
}

fn reject_running_instance(layout: &Layout) -> Result<()> {
    let broker = layout.run_dir.join("broker.sock");
    if broker.exists() && runner::connect_broker(&broker).is_ok() {
        bail!("target instance is running: {}", layout.instance);
    }
    if layout.run_dir.join("bootstrap.lock.d").exists() {
        bail!("target instance is starting: {}", layout.instance);
    }
    Ok(())
}

fn start_existing_instance(layout: &Layout, state: &AppState) -> Result<()> {
    match instance_state(layout) {
        "running" => return Ok(()),
        "starting" => bail!("instance is already starting: {}", layout.instance),
        "partial" => bail!("instance is missing rootfs: {}", layout.instance),
        _ => {}
    }
    if !layout.rootfs.exists() {
        bail!("instance rootfs is missing: {}", layout.rootfs.display());
    }
    let mut command =
        build_instance_start_command(layout, state, Some(60 * 60 * 1000), &["true".to_string()])?;
    let output = command.output().context("start sandbox")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        if message.is_empty() {
            bail!(
                "start exited with status {}: {}",
                output.status,
                layout.instance
            );
        }
        bail!("{message}");
    }
    Ok(())
}

fn start_imported_instance(
    layout: &Layout,
    options: &ImportOptions,
    state: &AppState,
) -> Result<i32> {
    let command = if options.command.is_empty() {
        vec!["true".to_string()]
    } else {
        options.command.clone()
    };
    run_instance_start_command(layout, state, options.idle_ttl_ms, &command)
}

fn run_instance_start_command(
    layout: &Layout,
    state: &AppState,
    idle_ttl_ms: Option<u64>,
    guest_command: &[String],
) -> Result<i32> {
    let mut command = build_instance_start_command(layout, state, idle_ttl_ms, guest_command)?;
    let status = command.status().context("start sandbox")?;
    Ok(status.code().unwrap_or(1))
}

fn build_instance_start_command(
    layout: &Layout,
    state: &AppState,
    idle_ttl_ms: Option<u64>,
    guest_command: &[String],
) -> Result<Command> {
    let exe = std::env::current_exe().context("current executable")?;
    let config = descriptor::load(layout)?;
    let cpus = config.cpus.unwrap_or(state.cpus);
    let memory_mib = config.memory_mib.unwrap_or(state.memory_mib);
    let mut command = Command::new(exe);
    command
        .arg("--instance")
        .arg(&layout.instance)
        .arg("--cpus")
        .arg(cpus.to_string())
        .arg("--memory-mib")
        .arg(memory_mib.to_string());
    if state.nested_kvm {
        command.arg("--nested-kvm");
    }
    if state.no_host_shares {
        command.arg("--no-host-shares");
    }
    if let Some(ttl) = idle_ttl_ms {
        command.env("LNX_BROKER_IDLE_TTL_MS", ttl.to_string());
    }
    command.args(guest_command);
    Ok(command)
}

async fn stop_existing_instance(layout: &Layout) -> Result<()> {
    stop_existing_instance_with_timeout(layout, Duration::from_secs(120)).await
}

async fn stop_existing_instance_with_timeout(layout: &Layout, timeout: Duration) -> Result<()> {
    if !layout.instance_dir.exists() && !layout.snapshot_dir.exists() && !layout.run_dir.exists() {
        return Ok(());
    }
    let deadline = std::time::Instant::now() + timeout;
    let lock_dir = layout.run_dir.join("bootstrap.lock.d");
    let mut signaled_pid = alive_owner_pid(&lock_dir);
    if let Some(pid) = signaled_pid {
        // The process that launched the initial command keeps owner-start.lock
        // until that command returns. Signal the established owner first so
        // shutdown can release that client and, in turn, the start lock.
        signal_owner_process(pid)?;
    }
    let start_lock_path = layout.run_dir.join("owner-start.lock.d");
    let _start_lock = loop {
        if let Some(lock) = runner::OwnerStartLock::try_acquire(&start_lock_path)? {
            break lock;
        }
        if let Some(pid) = alive_owner_pid(&lock_dir)
            && signaled_pid != Some(pid)
        {
            signal_owner_process(pid)?;
            signaled_pid = Some(pid);
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "instance {} did not finish starting within {} seconds; retry stop",
                layout.instance,
                timeout.as_secs_f64()
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let pid = loop {
        if let Some(pid) = alive_owner_pid(&lock_dir) {
            if signaled_pid != Some(pid) {
                signal_owner_process(pid)?;
            }
            break pid;
        }
        let (lock_exists, recorded_pid, maintenance_pid) = runner::with_lock_dir_guard(
            &lock_dir,
            || {
                let recorded_pid = runner::recorded_owner_pid_from_lock(&lock_dir).with_context(|| {
                format!(
                    "instance {} has a corrupt owner lease; recovery: lnx --instance {} snapshots clear to acknowledge and explicitly cold-boot",
                    layout.instance, layout.instance
                )
            })?;
                let maintenance_pid = runner::recorded_maintenance_pid_from_lock(&lock_dir)
                    .context("read instance state-copy lease")?;
                Ok((lock_dir.exists(), recorded_pid, maintenance_pid))
            },
        )?;
        if recorded_pid.is_some_and(process_alive) {
            continue;
        }
        if maintenance_pid.is_some_and(process_alive) {
            if std::time::Instant::now() >= deadline {
                bail!(
                    "instance {} state copy did not finish within {} seconds; retry stop",
                    layout.instance,
                    timeout.as_secs_f64()
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        }
        if lock_exists && recorded_pid.is_none() && maintenance_pid.is_none() {
            if std::time::Instant::now() >= deadline {
                bail!(
                    "instance {} owner lease remained incomplete at {} for {} seconds; run lnx --instance {} snapshots clear to acknowledge and explicitly cold-boot",
                    layout.instance,
                    lock_dir.display(),
                    timeout.as_secs_f64(),
                    layout.instance
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        }
        let expected_pid = signaled_pid.or(recorded_pid);
        ensure_shutdown_state_under_guard(layout, expected_pid)?;
        return Ok(());
    };
    while std::time::Instant::now() < deadline {
        match alive_owner_pid(&lock_dir) {
            None => {
                ensure_shutdown_state_under_guard(layout, Some(pid))?;
                return Ok(());
            }
            Some(current_pid) if current_pid != pid => {
                bail!(
                    "instance {} changed VM owner from pid {pid} to pid {current_pid} while stopping",
                    layout.instance
                );
            }
            Some(_) => {}
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    match alive_owner_pid(&lock_dir) {
        None => ensure_shutdown_state_under_guard(layout, Some(pid)),
        Some(current_pid) if current_pid != pid => bail!(
            "instance {} changed VM owner from pid {pid} to pid {current_pid} while stopping",
            layout.instance
        ),
        Some(_) => bail!(
            "owner process {pid} for instance {} did not finish its shutdown snapshot within {} seconds; it was left running so recoverable state is not discarded",
            layout.instance,
            timeout.as_secs_f64()
        ),
    }
}

fn ensure_shutdown_state_under_guard(layout: &Layout, expected_pid: Option<i32>) -> Result<()> {
    let lock_dir = layout.run_dir.join("bootstrap.lock.d");
    let checked = runner::with_lock_dir_guard(&lock_dir, || {
        if lock_dir.exists() && !runner::bootstrap_lock_is_stale(&lock_dir)? {
            return Ok(false);
        }
        ensure_no_failed_shutdown_state(layout, expected_pid)?;
        if lock_dir.exists() {
            fs::remove_dir_all(&lock_dir)
                .with_context(|| format!("remove {}", lock_dir.display()))?;
        }
        Ok(true)
    })?;
    if !checked {
        bail!(
            "instance {} acquired a new VM owner while shutdown was being verified; retry stop",
            layout.instance
        );
    }
    Ok(())
}

fn ensure_no_failed_shutdown_state(layout: &Layout, expected_pid: Option<i32>) -> Result<()> {
    if runner::restore_work_is_active(layout) {
        bail!(
            "instance {} stopped without publishing its final snapshot; recoverable state remains at {}\nrecovery: preserve it for inspection, or run lnx --instance {} snapshots clear to explicitly discard it",
            layout.instance,
            layout
                .snapshot_dir
                .join(runner::RESTORE_WORK_SNAPSHOT)
                .display(),
            layout.instance
        );
    }
    let expected_pid = expected_pid
        .map(u32::try_from)
        .transpose()
        .context("owner pid does not fit final snapshot outcome")?;
    if let Some(pid) = expected_pid {
        runner::validate_or_record_final_snapshot_failure(layout, pid)
    } else {
        runner::validate_final_snapshot_outcome(layout, None)
    }
}

fn signal_owner_process(pid: i32) -> Result<()> {
    signal_process(pid, libc::SIGTERM)
}

fn signal_process(pid: i32, signal: i32) -> Result<()> {
    if pid <= 0 {
        bail!("invalid owner pid: {pid}");
    }
    let rc = unsafe { libc::kill(pid as libc::pid_t, signal) };
    if rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(std::io::Error::last_os_error()).with_context(|| format!("signal process {pid}"))
}

fn remove_path_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

pub fn push(config: PushConfig) -> Result<()> {
    validate_instance_name(&config.source.instance)?;
    validate_instance_name(&config.target_instance)?;
    if !config.source.instance_dir.exists() {
        bail!("source instance does not exist: {}", config.source.instance);
    }

    let PushSource { layout, _tempdir } = prepare_push_source(&config.source)?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(24 * 60 * 60))
        .build()
        .context("build HTTP client")?;
    push_cas_bundle(&client, &layout, &config)
}

fn push_cas_bundle(
    client: &reqwest::blocking::Client,
    layout: &Layout,
    config: &PushConfig,
) -> Result<()> {
    eprintln!("scanning sandbox blocks");
    let bundle = CasPushBundle::open(layout, config)?;
    let url = cas_upload_url(&config.url, &config.target_instance)?;
    let manifest_json =
        serde_json::to_vec(&bundle.manifest).context("encode CAS upload manifest")?;
    let response = client
        .post(url)
        .header("content-type", "application/json")
        .body(manifest_json)
        .send()
        .context("start CAS upload")?;
    let status = response.status();
    let text = response.text().context("read CAS upload response")?;
    if !status.is_success() {
        bail!("server returned {status}: {}", server_error_message(&text));
    }
    let start: CasUploadStartResponse =
        serde_json::from_str(&text).context("parse CAS upload response")?;
    eprintln!(
        "dedupe: {} known blocks, {} missing blocks, {} logical, {} to upload",
        start.known_blocks,
        start.missing_blocks,
        human_bytes(start.logical_bytes),
        human_bytes(start.missing_bytes)
    );
    let result = (|| {
        if !start.missing.is_empty() {
            let mut payload = tempfile::Builder::new()
                .prefix("lnx-cas-block-stream-")
                .tempfile()
                .context("create CAS block stream payload")?;
            let mut packed = 0u64;
            let pack_started = std::time::Instant::now();
            {
                let mut encoder = zstd::stream::write::Encoder::new(payload.as_file_mut(), 3)
                    .context("create CAS block stream encoder")?;
                for hash in &start.missing {
                    let Some(block) = bundle.blocks.get(hash) else {
                        bail!("server requested unknown local CAS block {hash}");
                    };
                    let raw = block.read()?;
                    write_cas_block_frame(&mut encoder, hash, &raw)?;
                    packed = packed.saturating_add(block.len);
                    draw_progress(
                        "packing missing blocks",
                        packed,
                        start.missing_bytes,
                        pack_started,
                    );
                }
                encoder.finish().context("finish CAS block stream")?;
            }
            eprintln!();
            let compressed_bytes = payload
                .as_file()
                .metadata()
                .context("stat CAS block stream payload")?
                .len();
            let url = cas_blocks_url(&config.url, &start.session)?;
            let reader = ProgressReader::new(
                payload
                    .reopen()
                    .context("reopen CAS block stream payload")?,
                compressed_bytes,
                "uploading packed blocks",
            );
            let response = client
                .put(url)
                .header("content-encoding", "zstd")
                .body(reqwest::blocking::Body::sized(reader, compressed_bytes))
                .send()
                .context("upload CAS block stream")?;
            eprintln!();
            let status = response.status();
            let text = response.text().context("read CAS block stream response")?;
            if !status.is_success() {
                bail!("server returned {status}: {}", server_error_message(&text));
            }
            let stored: CasBlocksResponse =
                serde_json::from_str(&text).context("parse CAS block stream response")?;
            eprintln!("server stored {} blocks", stored.blocks);
        }
        eprintln!("committing import");
        let url = cas_commit_url(&config.url, &start.session)?;
        let response = client.post(url).send().context("commit CAS upload")?;
        let status = response.status();
        let text = response.text().context("read CAS commit response")?;
        if !status.is_success() {
            bail!("server returned {status}: {}", server_error_message(&text));
        }
        println!("{text}");
        Ok(())
    })();
    if result.is_err() {
        if let Ok(url) = cas_session_url(&config.url, &start.session) {
            let _ = client.delete(url).send();
        }
    }
    result
}

struct PushSource {
    layout: Layout,
    _tempdir: Option<tempfile::TempDir>,
}

struct CasPushBundle {
    manifest: CasUploadManifest,
    blocks: BTreeMap<String, CasBlockSource>,
}

#[derive(Clone)]
struct CasBlockSource {
    path: PathBuf,
    offset: u64,
    len: u64,
}

impl CasBlockSource {
    fn read(&self) -> Result<Vec<u8>> {
        let mut file =
            fs::File::open(&self.path).with_context(|| format!("open {}", self.path.display()))?;
        file.seek(SeekFrom::Start(self.offset))
            .with_context(|| format!("seek {}", self.path.display()))?;
        let mut buf = vec![0u8; self.len as usize];
        file.read_exact(&mut buf)
            .with_context(|| format!("read {}", self.path.display()))?;
        Ok(buf)
    }
}

impl CasPushBundle {
    fn open(layout: &Layout, config: &PushConfig) -> Result<Self> {
        let files = collect_sparse_bundle_files(layout)?;
        let mut manifest_files = Vec::with_capacity(files.len());
        let mut blocks = BTreeMap::new();
        let zero_hash = zero_block_hash();
        for file in files {
            let mut manifest_blocks = Vec::new();
            let mut source = fs::File::open(&file.source)
                .with_context(|| format!("open {}", file.source.display()))?;
            let mut buf = vec![0u8; CAS_BLOCK_SIZE as usize];
            for extent in &file.extents {
                let mut remaining = extent.len;
                let mut offset = extent.offset;
                while remaining > 0 {
                    let len = remaining.min(CAS_BLOCK_SIZE) as usize;
                    source
                        .seek(SeekFrom::Start(offset))
                        .with_context(|| format!("seek {}", file.source.display()))?;
                    source
                        .read_exact(&mut buf[..len])
                        .with_context(|| format!("read {}", file.source.display()))?;
                    let sha256 = if len as u64 == CAS_BLOCK_SIZE
                        && buf[..len].iter().all(|byte| *byte == 0)
                    {
                        zero_hash.clone()
                    } else {
                        sha256_hex(&buf[..len])
                    };
                    manifest_blocks.push(CasManifestBlock {
                        offset,
                        len: len as u64,
                        sha256: sha256.clone(),
                    });
                    blocks.entry(sha256).or_insert_with(|| CasBlockSource {
                        path: file.source.clone(),
                        offset,
                        len: len as u64,
                    });
                    remaining -= len as u64;
                    offset += len as u64;
                }
            }
            manifest_files.push(CasManifestFile {
                path: file.relative,
                len: file.len,
                mode: file.mode,
                blocks: manifest_blocks,
            });
        }
        Ok(Self {
            manifest: CasUploadManifest {
                version: 2,
                source_instance: layout.instance.clone(),
                replace: config.replace,
                start: config.start,
                idle_ttl_ms: config.idle_ttl_ms,
                command: config.command.clone(),
                files: manifest_files,
            },
            blocks,
        })
    }
}

#[cfg(test)]
struct SparseBundle {
    reader: SparseBundleReader,
    total_len: u64,
    _payload: tempfile::NamedTempFile,
}

#[cfg(test)]
struct SparseBundleReader {
    prefix: Cursor<Vec<u8>>,
    payload: fs::File,
}

#[cfg(test)]
impl SparseBundle {
    fn open(layout: &Layout) -> Result<Self> {
        let mut files = collect_sparse_bundle_files(layout)?;
        let mut payload = tempfile::NamedTempFile::new().context("create sparse bundle payload")?;
        let compressed_bytes = compress_sparse_bundle_payload(&mut files, payload.as_file_mut())?;
        let manifest = SparseBundleManifest {
            version: 1,
            source_instance: layout.instance.clone(),
            compression: Some("zstd".to_string()),
            files: files
                .iter()
                .map(|file| SparseBundleFile {
                    path: file.relative.clone(),
                    len: file.len,
                    mode: file.mode,
                    extents: file.extents.clone(),
                })
                .collect(),
        };
        let manifest_json =
            serde_json::to_vec(&manifest).context("encode sparse bundle manifest")?;
        let mut prefix = Vec::with_capacity(SPARSE_BUNDLE_MAGIC.len() + 8 + manifest_json.len());
        prefix.extend_from_slice(SPARSE_BUNDLE_MAGIC);
        prefix.extend_from_slice(&(manifest_json.len() as u64).to_le_bytes());
        prefix.extend_from_slice(&manifest_json);
        let total_len = (prefix.len() as u64)
            .checked_add(compressed_bytes)
            .context("sparse bundle length overflow")?;
        let payload_reader = payload
            .reopen()
            .context("open sparse bundle compressed payload")?;
        Ok(Self {
            reader: SparseBundleReader {
                prefix: Cursor::new(prefix),
                payload: payload_reader,
            },
            total_len,
            _payload: payload,
        })
    }
}

struct SparseBundleCollectedFile {
    relative: String,
    source: std::path::PathBuf,
    len: u64,
    mode: u32,
    extents: Vec<SparseExtent>,
}

#[cfg(test)]
impl Read for SparseBundleReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if (self.prefix.position() as usize) < self.prefix.get_ref().len() {
            return self.prefix.read(buf);
        }
        self.payload.read(buf)
    }
}

#[cfg(test)]
fn compress_sparse_bundle_payload(
    files: &mut [SparseBundleCollectedFile],
    payload: &mut fs::File,
) -> Result<u64> {
    let mut compressed_total = 0u64;
    for file in files {
        let mut source = fs::File::open(&file.source)
            .with_context(|| format!("open {}", file.source.display()))?;
        for extent in &mut file.extents {
            source
                .seek(SeekFrom::Start(extent.offset))
                .with_context(|| format!("seek {}", file.source.display()))?;
            let start = payload
                .stream_position()
                .context("read sparse bundle payload position")?;
            {
                let mut encoder = zstd::stream::write::Encoder::new(&mut *payload, 3)
                    .context("create zstd encoder")?;
                let mut limited = std::io::Read::by_ref(&mut source).take(extent.len);
                let copied = std::io::copy(&mut limited, &mut encoder)
                    .with_context(|| format!("compress {}", file.source.display()))?;
                if copied != extent.len {
                    bail!(
                        "source file ended inside sparse extent {} at {}",
                        file.source.display(),
                        extent.offset
                    );
                }
                encoder.finish().context("finish zstd extent")?;
            }
            let end = payload
                .stream_position()
                .context("read sparse bundle payload position")?;
            let compressed_len = end
                .checked_sub(start)
                .context("compressed extent position underflow")?;
            extent.compressed_len = Some(compressed_len);
            compressed_total = compressed_total
                .checked_add(compressed_len)
                .context("compressed sparse bundle payload size overflow")?;
        }
    }
    payload
        .seek(SeekFrom::Start(0))
        .context("rewind sparse bundle payload")?;
    Ok(compressed_total)
}

fn collect_sparse_bundle_files(layout: &Layout) -> Result<Vec<SparseBundleCollectedFile>> {
    let mut files = Vec::new();
    if layout.kernel == layout.base.join("vmlinuz") && layout.kernel.exists() {
        collect_sparse_bundle_file(&layout.base, &layout.kernel, &mut files)?;
    }
    collect_sparse_bundle_tree(layout, &layout.base, &layout.instance_dir, &mut files)?;
    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(files)
}

fn collect_sparse_bundle_tree(
    layout: &Layout,
    base: &Path,
    path: &Path,
    files: &mut Vec<SparseBundleCollectedFile>,
) -> Result<()> {
    if bundle_runtime_path_is_excluded(layout, path) {
        return Ok(());
    }
    let meta = fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if meta.is_dir() {
        for entry in fs::read_dir(path).with_context(|| format!("read {}", path.display()))? {
            collect_sparse_bundle_tree(layout, base, &entry?.path(), files)?;
        }
        return Ok(());
    }
    if !meta.is_file() {
        bail!(
            "cannot include non-regular file in sparse bundle: {}",
            path.display()
        );
    }
    collect_sparse_bundle_file(base, path, files)
}

fn bundle_runtime_path_is_excluded(layout: &Layout, path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let runtime_exact = [
        "bootstrap.lock.d",
        "bootstrap.lock.d.guard",
        "owner-start.lock.d",
        "owner-start.lock.d.guard",
        "broker.sock",
        "checkpoint-broker.sock",
        "lnx-agent.sock",
        "lnx-snapshot.sock",
        "lnx-control.sock",
        "gvproxy.sock",
    ]
    .into_iter()
    .any(|runtime| path == layout.run_dir.join(runtime));
    let runtime_lock_descendant = path.starts_with(layout.run_dir.join("bootstrap.lock.d"))
        || path.starts_with(layout.run_dir.join("owner-start.lock.d"));
    let runtime_socket =
        path.parent() == Some(layout.run_dir.as_path()) && name.ends_with("-krun.sock");
    let persistent_transaction_file = path.parent() == Some(layout.instance_dir.as_path())
        && (name == ".lnx-fork-lease" || name.starts_with(".lnx-descriptor-"));
    let snapshot_runtime = path.parent() == Some(layout.snapshot_dir.as_path())
        && (matches!(
            name,
            ".restore-work" | ".restore-work.active" | ".latest.next" | ".latest.previous"
        ) || (name.starts_with('.') && name.contains(".clear-"))
            || name.starts_with(".final-snapshot.outcome.tmp-"));
    runtime_exact
        || runtime_lock_descendant
        || runtime_socket
        || persistent_transaction_file
        || snapshot_runtime
}

fn collect_sparse_bundle_file(
    base: &Path,
    path: &Path,
    files: &mut Vec<SparseBundleCollectedFile>,
) -> Result<()> {
    let metadata = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let relative = path
        .strip_prefix(base)
        .with_context(|| format!("make {} relative to {}", path.display(), base.display()))?;
    let relative = relative
        .to_str()
        .with_context(|| format!("bundle path is not UTF-8: {}", relative.display()))?
        .to_string();
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o7777
    };
    #[cfg(not(unix))]
    let mode = 0o644;
    files.push(SparseBundleCollectedFile {
        relative,
        source: path.to_path_buf(),
        len: metadata.len(),
        mode,
        extents: sparse_file_extents(path, &metadata)?,
    });
    Ok(())
}

fn sparse_file_extents(path: &Path, metadata: &fs::Metadata) -> Result<Vec<SparseExtent>> {
    let len = metadata.len();
    if len == 0 {
        return Ok(Vec::new());
    }
    #[cfg(unix)]
    let allocated = {
        use std::os::unix::fs::MetadataExt;
        metadata.blocks() * 512
    };
    #[cfg(not(unix))]
    let allocated = len;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        match seek_data_extents(path, len) {
            Ok(extents) => {
                if is_suspicious_dense_extent_report(len, allocated, &extents) {
                    bail!(
                        "filesystem reports {} as one dense extent; refusing to expand sparse image",
                        path.display()
                    );
                }
                return Ok(extents);
            }
            Err(error) if is_large_sparse_transfer(len, allocated) => {
                return Err(error)
                    .with_context(|| format!("discover sparse extents for {}", path.display()));
            }
            Err(_) => {}
        }
    }

    Ok(vec![SparseExtent {
        offset: 0,
        len,
        compressed_len: None,
    }])
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn seek_data_extents(path: &Path, len: u64) -> Result<Vec<SparseExtent>> {
    use std::os::fd::AsRawFd;

    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut extents = Vec::new();
    let mut offset = 0u64;
    while offset < len {
        let data = unsafe { libc::lseek(file.as_raw_fd(), offset as libc::off_t, libc::SEEK_DATA) };
        if data < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENXIO) {
                break;
            }
            return Err(error).with_context(|| format!("seek data {}", path.display()));
        }
        let hole = unsafe { libc::lseek(file.as_raw_fd(), data, libc::SEEK_HOLE) };
        if hole < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("seek hole {}", path.display()));
        }
        let start = data as u64;
        let end = (hole as u64).min(len);
        if end > start {
            extents.push(SparseExtent {
                offset: start,
                len: end - start,
                compressed_len: None,
            });
        }
        offset = end;
    }
    Ok(extents)
}

fn is_large_sparse_transfer(len: u64, allocated: u64) -> bool {
    len >= 8 * 1024 * 1024 * 1024 && allocated <= len / 2
}

fn is_suspicious_dense_extent_report(len: u64, allocated: u64, extents: &[SparseExtent]) -> bool {
    is_large_sparse_transfer(len, allocated)
        && matches!(
            extents,
            [SparseExtent {
                offset: 0,
                len: extent_len,
                ..
            }] if *extent_len == len
        )
}

#[cfg(test)]
fn upload_progress_frame(elapsed: Duration) -> String {
    const WIDTH: usize = 20;
    const PULSE: usize = 5;
    let max_start = WIDTH - PULSE;
    let phase = ((elapsed.as_millis() / 80) as usize) % (max_start * 2);
    let start = if phase <= max_start {
        phase
    } else {
        max_start * 2 - phase
    };
    let mut frame = String::with_capacity(WIDTH);
    for i in 0..WIDTH {
        frame.push(if (start..start + PULSE).contains(&i) {
            '='
        } else {
            ' '
        });
    }
    frame
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

struct ProgressReader<R> {
    inner: R,
    total: u64,
    done: u64,
    label: &'static str,
    started: std::time::Instant,
}

impl<R> ProgressReader<R> {
    fn new(inner: R, total: u64, label: &'static str) -> Self {
        Self {
            inner,
            total,
            done: 0,
            label,
            started: std::time::Instant::now(),
        }
    }
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        if read != 0 {
            self.done = self.done.saturating_add(read as u64);
            draw_progress(self.label, self.done, self.total, self.started);
        }
        Ok(read)
    }
}

fn draw_progress(label: &str, done: u64, total: u64, started: std::time::Instant) {
    let elapsed = started.elapsed().as_secs_f64().max(0.001);
    let rate = (done as f64 / elapsed) as u64;
    let percent = if total == 0 {
        100.0
    } else {
        (done as f64 / total as f64 * 100.0).min(100.0)
    };
    eprint!(
        "\r{label} [{:>6.2}%] {} / {}, {}/s",
        percent,
        human_bytes(done),
        human_bytes(total),
        human_bytes(rate)
    );
    let _ = std::io::stderr().flush();
}

fn prepare_push_source(layout: &Layout) -> Result<PushSource> {
    let owner_is_live = runner::validate_restore_work_for_command(layout)?;
    if owner_is_live {
        eprintln!(
            "source instance {} is running; checkpointing before push",
            layout.instance
        );
        let tempdir = tempfile::Builder::new()
            .prefix("lnx-server-push-")
            .tempdir()
            .context("create temporary push bundle")?;
        let bundle = checkpoint_bundle_layout(layout, tempdir.path());
        materialize_running_source_checkpoint(layout, &bundle)?;
        return Ok(PushSource {
            layout: bundle,
            _tempdir: Some(tempdir),
        });
    }

    let tempdir = tempfile::Builder::new()
        .prefix("lnx-server-push-stopped-")
        .tempdir()
        .context("create stable stopped-source push bundle")?;
    let bundle = checkpoint_bundle_layout(layout, tempdir.path());
    let copied = runner::with_validated_stopped_instance(layout, || {
        materialize_stopped_push_copy(layout, &bundle)
    })?;
    if copied.is_none() {
        bail!(
            "source instance {} started while its push snapshot was being prepared; retry so it can be checkpointed coherently",
            layout.instance
        );
    }
    Ok(PushSource {
        layout: bundle,
        _tempdir: Some(tempdir),
    })
}

fn materialize_stopped_push_copy(source: &Layout, bundle: &Layout) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    for file in collect_sparse_bundle_files(source)? {
        let destination = bundle.base.join(&file.relative);
        sparse_copy::clone_or_copy_file(&file.source, &destination).with_context(|| {
            format!(
                "clone stable push source {} to {}",
                file.source.display(),
                destination.display()
            )
        })?;
        fs::set_permissions(&destination, fs::Permissions::from_mode(file.mode))
            .with_context(|| format!("set permissions on {}", destination.display()))?;
    }
    Ok(())
}

fn materialize_running_source_checkpoint(source: &Layout, bundle: &Layout) -> Result<()> {
    fs::create_dir_all(&source.checkpoint_dir)
        .with_context(|| format!("create {}", source.checkpoint_dir.display()))?;
    let (checkpoint, path) = checkpoints::new_checkpoint_path(source, None)?;
    let result = (|| {
        runner::request_coherent_checkpoint_awaiting_owner(source, &path, Duration::from_secs(120))
            .context("checkpoint running source before push")?;
        checkpoints::write_metadata(source, &checkpoint)?;
        materialize_checkpoint_bundle(source, &checkpoint, bundle)
    })();
    let cleanup = match fs::remove_dir_all(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    };
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), Err(cleanup_error)) => Err(error.context(format!(
            "also failed to clean temporary checkpoint: {cleanup_error:#}"
        ))),
    }
}

fn materialize_checkpoint_bundle(
    source: &Layout,
    checkpoint: &checkpoints::Checkpoint,
    bundle: &Layout,
) -> Result<()> {
    checkpoints::fork(source, checkpoint, bundle)
}

fn checkpoint_bundle_layout(source: &Layout, base: &Path) -> Layout {
    Layout {
        base: base.to_path_buf(),
        instance: source.instance.clone(),
        kernel: base.join("vmlinuz"),
        rootfs: base
            .join("instances")
            .join(&source.instance)
            .join("rootfs.ext4"),
        instance_dir: base.join("instances").join(&source.instance),
        snapshot_dir: base
            .join("instances")
            .join(&source.instance)
            .join("memory-snapshots"),
        checkpoint_dir: base
            .join("instances")
            .join(&source.instance)
            .join("checkpoints"),
        vm_initialized: base
            .join("instances")
            .join(&source.instance)
            .join("vm-initialized"),
        run_dir: base.join("instances").join(&source.instance),
        console_log: base
            .join("instances")
            .join(&source.instance)
            .join("console.log"),
    }
}

#[cfg(test)]
fn sandbox_url(base: &str, instance: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base).with_context(|| format!("parse server URL {base}"))?;
    url.set_path(&format!("/v1/sandboxes/{instance}"));
    Ok(url)
}

fn cas_upload_url(base: &str, instance: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base).with_context(|| format!("parse server URL {base}"))?;
    url.set_path(&format!("/v2/sandboxes/{instance}/uploads"));
    Ok(url)
}

fn cas_blocks_url(base: &str, session: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base).with_context(|| format!("parse server URL {base}"))?;
    url.set_path(&format!("/v2/uploads/{session}/blocks"));
    Ok(url)
}

fn cas_commit_url(base: &str, session: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base).with_context(|| format!("parse server URL {base}"))?;
    url.set_path(&format!("/v2/uploads/{session}/commit"));
    Ok(url)
}

fn cas_session_url(base: &str, session: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base).with_context(|| format!("parse server URL {base}"))?;
    url.set_path(&format!("/v2/uploads/{session}"));
    Ok(url)
}

fn server_error_message(text: &str) -> String {
    #[derive(Deserialize)]
    struct ErrorBody {
        message: Option<String>,
    }
    serde_json::from_str::<ErrorBody>(text)
        .ok()
        .and_then(|body| body.message)
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| text.to_string())
}

fn server_base() -> Result<PathBuf> {
    Ok(Layout::resolve("__server__", None, None)?.base)
}

fn cas_root() -> Result<PathBuf> {
    Ok(server_base()?.join("cas").join("sha256"))
}

fn cas_uploads_root() -> Result<PathBuf> {
    Ok(server_base()?.join("uploads"))
}

fn cas_session_dir(session: &str) -> Result<PathBuf> {
    validate_session_id(session)?;
    Ok(cas_uploads_root()?.join(session))
}

fn cas_block_path(sha256: &str) -> Result<PathBuf> {
    validate_sha256_hex(sha256)?;
    Ok(cas_root()?.join(&sha256[..2]).join(sha256))
}

fn store_cas_block(sha256: &str, raw: &[u8]) -> Result<()> {
    validate_sha256_hex(sha256)?;
    if raw.len() as u64 > CAS_BLOCK_SIZE {
        bail!("CAS block is too large: {} bytes", raw.len());
    }
    let actual = sha256_hex(raw);
    if actual != sha256 {
        bail!("CAS block hash mismatch: expected {sha256}, got {actual}");
    }
    if sha256 == zero_block_hash() {
        return Ok(());
    }
    let path = cas_block_path(sha256)?;
    if path.exists() {
        return Ok(());
    }
    let parent = path.parent().context("CAS block path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temp, raw).with_context(|| format!("write {}", temp.display()))?;
    match fs::hard_link(&temp, &path) {
        Ok(()) => {
            let _ = fs::remove_file(&temp);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&temp);
            Ok(())
        }
        Err(_) => fs::rename(&temp, &path)
            .with_context(|| format!("move CAS block {} to {}", temp.display(), path.display())),
    }
}

fn store_cas_block_stream(input: &mut dyn Read) -> Result<usize> {
    let mut count = 0usize;
    loop {
        let mut hash_bytes = [0u8; 32];
        match input.read_exact(&mut hash_bytes) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e).context("read CAS block stream hash"),
        }
        let len = read_u32(input).context("read CAS block stream length")? as usize;
        if len == 0 || len as u64 > CAS_BLOCK_SIZE {
            bail!("invalid CAS block stream length: {len}");
        }
        let mut raw = vec![0u8; len];
        input
            .read_exact(&mut raw)
            .context("read CAS block stream payload")?;
        let sha256 = hex_from_bytes(&hash_bytes);
        store_cas_block(&sha256, &raw)?;
        count += 1;
    }
    Ok(count)
}

fn write_cas_block_frame(output: &mut dyn Write, sha256: &str, raw: &[u8]) -> Result<()> {
    validate_sha256_hex(sha256)?;
    if raw.len() as u64 > CAS_BLOCK_SIZE {
        bail!("CAS block is too large: {} bytes", raw.len());
    }
    output
        .write_all(&bytes_from_hex(sha256)?)
        .context("write CAS block frame hash")?;
    output
        .write_all(&(raw.len() as u32).to_le_bytes())
        .context("write CAS block frame length")?;
    output.write_all(raw).context("write CAS block frame data")
}

fn read_u32(input: &mut dyn Read) -> std::io::Result<u32> {
    let mut bytes = [0u8; 4];
    input.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_from_bytes(&digest)
}

fn hex_from_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn bytes_from_hex(value: &str) -> Result<[u8; 32]> {
    validate_sha256_hex(value)?;
    let mut bytes = [0u8; 32];
    for i in 0..32 {
        bytes[i] = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16)
            .with_context(|| format!("parse sha256 hex: {value}"))?;
    }
    Ok(bytes)
}

fn zero_block_hash() -> String {
    sha256_hex(&vec![0u8; CAS_BLOCK_SIZE as usize])
}

fn validate_sha256_hex(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid sha256 hex: {value}");
    }
    Ok(())
}

fn validate_session_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || value
            .bytes()
            .any(|b| !matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_'))
    {
        bail!("invalid upload session id: {value}");
    }
    Ok(())
}

fn new_upload_session_id(target_instance: &str) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("host clock is before Unix epoch")?
        .as_nanos();
    let seed = format!("{target_instance}:{now}:{}", std::process::id());
    Ok(format!(
        "{}-{}",
        target_instance,
        &sha256_hex(seed.as_bytes())[..16]
    ))
}

pub(crate) fn validate_instance_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    {
        bail!("invalid instance name: {name}");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
