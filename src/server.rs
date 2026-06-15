use std::{
    fs,
    io::{BufRead, BufReader},
    net::SocketAddr,
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, put},
};
use futures_util::StreamExt;
use serde::Serialize;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;

use crate::{descriptor, paths::Layout, runner, sparse_copy};

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

#[derive(Serialize)]
struct ImportResponse {
    ok: bool,
    instance: String,
    imported: String,
    started: bool,
    status: Option<i32>,
}

#[derive(Debug)]
struct ImportOptions {
    source_instance: String,
    replace: bool,
    start: bool,
    idle_ttl_ms: Option<u64>,
    command: Vec<String>,
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
        .route("/v1/sandboxes/{instance}", put(import_sandbox))
        .with_state(state);
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

async fn import_sandbox(
    State(state): State<Arc<AppState>>,
    AxumPath(instance): AxumPath<String>,
    headers: HeaderMap,
    body: Body,
) -> std::result::Result<Json<ImportResponse>, ApiError> {
    validate_instance_name(&instance).map_err(ApiError::bad_request)?;
    let options = import_options(&headers).map_err(ApiError::bad_request)?;
    validate_instance_name(&options.source_instance).map_err(ApiError::bad_request)?;
    let archive = tempfile::NamedTempFile::new().map_err(ApiError::internal)?;
    let archive_path = archive.path().to_path_buf();
    write_body_to_file(body, &archive_path).await?;

    let import_state = (*state).clone();
    let target = instance.clone();
    let result = tokio::task::spawn_blocking(move || {
        import_archive(&archive_path, &target, options, import_state)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)?;

    Ok(Json(result))
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

fn import_archive_to_layout(
    archive: &Path,
    dest: &Layout,
    target_instance: &str,
    options: ImportOptions,
    state: AppState,
) -> Result<ImportResponse> {
    reject_running_instance(&dest)?;
    if (dest.rootfs.exists() || dest.snapshot_dir.exists()) && !options.replace {
        bail!("target instance already exists: {target_instance} (use --replace)");
    }

    let temp = TempDir::new().context("create import tempdir")?;
    extract_archive(archive, temp.path())?;
    let imported = temp.path().join("instances").join(&options.source_instance);
    if !imported.join("rootfs.ext4").exists() {
        bail!(
            "sandbox bundle is missing instances/{}/rootfs.ext4",
            options.source_instance
        );
    }

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

fn start_imported_instance(
    layout: &Layout,
    options: &ImportOptions,
    state: &AppState,
) -> Result<i32> {
    let exe = std::env::current_exe().context("current executable")?;
    let mut command = Command::new(exe);
    command
        .arg("--instance")
        .arg(&layout.instance)
        .arg("--cpus")
        .arg(state.cpus.to_string())
        .arg("--memory-mib")
        .arg(state.memory_mib.to_string());
    if state.nested_kvm {
        command.arg("--nested-kvm");
    }
    if state.no_host_shares {
        command.arg("--no-host-shares");
    }
    if let Some(ttl) = options.idle_ttl_ms {
        command.env("LNX_BROKER_IDLE_TTL_MS", ttl.to_string());
    }
    if options.command.is_empty() {
        command.arg("true");
    } else {
        command.args(&options.command);
    }
    let status = command.status().context("start imported sandbox")?;
    Ok(status.code().unwrap_or(1))
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

    let mut tar = archive_source(&config.source)?;
    let stdout = tar.stdout.take().context("open tar stdout")?;
    let url = sandbox_url(&config.url, &config.target_instance)?;
    let command_json = serde_json::to_string(&config.command).context("encode command")?;
    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(24 * 60 * 60))
        .build()
        .context("build HTTP client")?
        .put(url)
        .header("content-type", "application/x-tar")
        .header("x-lnx-source-instance", &config.source.instance)
        .header("x-lnx-replace", bool_header(config.replace))
        .header("x-lnx-start", bool_header(config.start))
        .header("x-lnx-command-json", command_json)
        .header(
            "x-lnx-idle-ttl-ms",
            config
                .idle_ttl_ms
                .map(|ttl| ttl.to_string())
                .unwrap_or_default(),
        )
        .body(reqwest::blocking::Body::new(stdout))
        .send()
        .context("send sandbox bundle")?;
    let status = response.status();
    let text = response.text().context("read server response")?;
    let tar_status = tar.wait().context("wait for tar archive")?;
    if !tar_status.success() {
        bail!("tar archive failed with status {tar_status}");
    }
    if !status.is_success() {
        bail!("server returned {status}: {text}");
    }
    println!("{text}");
    Ok(())
}

fn archive_source(layout: &Layout) -> Result<std::process::Child> {
    let mut command = Command::new("tar");
    command.env("COPYFILE_DISABLE", "1");
    command.arg("-cf").arg("-").arg("-C").arg(&layout.base);
    if layout.kernel == layout.base.join("vmlinuz") && layout.kernel.exists() {
        command.arg("vmlinuz");
    }
    command
        .arg("-C")
        .arg(&layout.base)
        .arg(format!("instances/{}", layout.instance))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("start tar archive")?;
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(|line| line.ok()) {
                eprintln!("tar: {line}");
            }
        });
    }
    Ok(child)
}

fn sandbox_url(base: &str, instance: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base).with_context(|| format!("parse server URL {base}"))?;
    url.set_path(&format!("/v1/sandboxes/{instance}"));
    Ok(url)
}

fn bool_header(value: bool) -> &'static str {
    if value { "1" } else { "0" }
}

fn validate_instance_name(name: &str) -> Result<()> {
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
mod tests {
    use super::*;

    #[test]
    fn rejects_path_like_instance_names() {
        assert!(validate_instance_name("ok-name_1.2").is_ok());
        assert!(validate_instance_name("../nope").is_err());
        assert!(validate_instance_name("bad/name").is_err());
        assert!(validate_instance_name("").is_err());
    }

    #[test]
    fn builds_sandbox_url() {
        let url = sandbox_url("http://127.0.0.1:7777/base", "remote").expect("url");
        assert_eq!(url.as_str(), "http://127.0.0.1:7777/v1/sandboxes/remote");
    }

    #[test]
    fn imports_bundle_into_target_layout() {
        let source = TempDir::new().expect("source tempdir");
        let dest_base = TempDir::new().expect("dest tempdir");
        fs::create_dir_all(
            source
                .path()
                .join("instances/source/memory-snapshots/latest"),
        )
        .expect("create source dirs");
        fs::write(source.path().join("vmlinuz"), b"kernel").expect("kernel");
        fs::write(
            source.path().join("instances/source/rootfs.ext4"),
            b"rootfs",
        )
        .expect("rootfs");
        fs::write(
            source
                .path()
                .join("instances/source/memory-snapshots/latest/vmstate.bin"),
            b"vmstate",
        )
        .expect("vmstate");
        fs::write(
            source.path().join("instances/source/lnx.json"),
            br#"{"name":"source"}"#,
        )
        .expect("descriptor");
        fs::write(
            source.path().join("instances/source/vm-initialized"),
            b"1\n",
        )
        .expect("vm init");

        let archive = tempfile::NamedTempFile::new().expect("archive");
        let status = Command::new("tar")
            .arg("-C")
            .arg(source.path())
            .arg("-cf")
            .arg(archive.path())
            .arg("vmlinuz")
            .arg("instances/source")
            .status()
            .expect("tar");
        assert!(status.success());

        let dest = test_layout(dest_base.path(), "target");
        let response = import_archive_to_layout(
            archive.path(),
            &dest,
            "target",
            ImportOptions {
                source_instance: "source".to_string(),
                replace: false,
                start: false,
                idle_ttl_ms: None,
                command: Vec::new(),
            },
            AppState {
                cpus: 2,
                memory_mib: 1024,
                nested_kvm: false,
                no_host_shares: false,
            },
        )
        .expect("import");

        assert!(response.ok);
        assert_eq!(fs::read(&dest.rootfs).expect("read rootfs"), b"rootfs");
        assert_eq!(fs::read(&dest.kernel).expect("read kernel"), b"kernel");
        assert!(dest.snapshot_dir.join("latest/vmstate.bin").exists());
        assert_eq!(
            descriptor::load(&dest)
                .expect("load descriptor")
                .name
                .as_deref(),
            Some("target")
        );
    }

    fn test_layout(base: &Path, instance: &str) -> Layout {
        Layout {
            base: base.to_path_buf(),
            instance: instance.to_string(),
            kernel: base.join("vmlinuz"),
            rootfs: base.join("instances").join(instance).join("rootfs.ext4"),
            instance_dir: base.join("instances").join(instance),
            snapshot_dir: base
                .join("instances")
                .join(instance)
                .join("memory-snapshots"),
            checkpoint_dir: base.join("instances").join(instance).join("checkpoints"),
            vm_initialized: base.join("instances").join(instance).join("vm-initialized"),
            run_dir: base.join("instances").join(instance),
            console_log: base.join("instances").join(instance).join("console.log"),
        }
    }
}
