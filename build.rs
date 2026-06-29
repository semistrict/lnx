use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let tracked_paths = tracked_source_paths();
    for path in &tracked_paths {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    if server_ui_enabled() {
        let server_ui_paths = tracked_server_ui_paths();
        for path in &server_ui_paths {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    println!("cargo:rerun-if-env-changed=LNX_AGENT_TARGET_DIR");
    println!("cargo:rerun-if-env-changed=LNX_AGENT_TARGET");
    println!("cargo:rerun-if-env-changed=LNX_AGENT_LINKER");
    println!("cargo:rerun-if-env-changed=LNX_SKIP_SERVER_UI_BUILD");
    println!("cargo:rerun-if-env-changed=LNX_SKIP_SERVER_UI_INSTALL");

    let source_stamp = source_stamp(&tracked_paths);
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    if server_ui_enabled() {
        build_server_ui(&out_dir);
    }
    if target_os() == "macos" {
        build_gvproxy_bridge(&out_dir);
    }
    let agent = out_dir.join("lnx-agent");
    let agent_target_dir = env::var_os("LNX_AGENT_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| out_dir.join("guest-agent-target"));
    let target =
        env::var("LNX_AGENT_TARGET").unwrap_or_else(|_| "aarch64-unknown-linux-musl".to_string());
    let linker = env::var("LNX_AGENT_LINKER").unwrap_or_else(|_| "rust-lld".to_string());

    let mut command = Command::new("cargo");
    command
        .env(
            format!(
                "CARGO_TARGET_{}_LINKER",
                target.replace('-', "_").to_uppercase()
            ),
            linker,
        )
        .args([
            "build",
            "--manifest-path",
            "guest-agent/Cargo.toml",
            "--release",
            "--target",
            &target,
        ])
        .env("CARGO_TARGET_DIR", &agent_target_dir)
        .env(
            "RUSTFLAGS",
            format!(
                "-C target-feature=+crt-static --cfg=lnx_agent_source_stamp=\"{source_stamp}\""
            ),
        );
    let status = command.status().expect("run cargo for guest agent");
    if !status.success() {
        panic!("guest agent build failed with {status}");
    }
    let built = agent_target_dir
        .join(&target)
        .join("release")
        .join("lnx-agent");
    std::fs::copy(&built, &agent).unwrap_or_else(|e| {
        panic!(
            "copy guest agent {} to {}: {e}",
            built.display(),
            agent.display()
        )
    });

    println!("cargo:rustc-env=LNX_AGENT={}", agent.display());
    println!("cargo:rustc-env=LNX_AGENT_SOURCE_STAMP={source_stamp}");
}

fn target_os() -> String {
    env::var("CARGO_CFG_TARGET_OS").unwrap_or_default()
}

fn target_arch() -> String {
    env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default()
}

fn build_gvproxy_bridge(out_dir: &Path) {
    println!("cargo:rerun-if-changed=third_party/gvproxy-bridge/bridge.go");
    println!("cargo:rerun-if-changed=third_party/gvproxy-bridge/go.mod");
    println!("cargo:rerun-if-changed=third_party/gvproxy-bridge/go.sum");
    let goarch = match target_arch().as_str() {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => panic!("unsupported gvproxy bridge target arch {other}"),
    };
    let archive = out_dir.join("liblnx_gvproxy_bridge.a");
    let status = Command::new("go")
        .arg("build")
        .arg("-buildmode=c-archive")
        .arg("-o")
        .arg(&archive)
        .arg(".")
        .env("GOOS", "darwin")
        .env("GOARCH", goarch)
        .env("CGO_ENABLED", "1")
        .current_dir("third_party/gvproxy-bridge")
        .status()
        .expect("run go build for embedded gvproxy bridge");
    if !status.success() {
        panic!("embedded gvproxy bridge build failed with {status}");
    }
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=lnx_gvproxy_bridge");
    println!("cargo:rustc-link-lib=dylib=resolv");
}

fn server_ui_enabled() -> bool {
    env::var_os("CARGO_FEATURE_SERVER_UI").is_some()
}

fn tracked_source_paths() -> Vec<PathBuf> {
    let mut paths = vec![
        PathBuf::from("guest-agent/Cargo.toml"),
        PathBuf::from("guest-agent/Cargo.lock"),
        PathBuf::from("lnx-protocol/Cargo.toml"),
    ];
    collect_files(Path::new("guest-agent/src"), &mut paths);
    collect_files(Path::new("lnx-protocol/src"), &mut paths);
    paths.sort();
    paths
}

fn collect_files(dir: &Path, paths: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, paths);
        } else {
            paths.push(path);
        }
    }
}

fn tracked_server_ui_paths() -> Vec<PathBuf> {
    let mut paths = vec![
        PathBuf::from("server-ui/index.html"),
        PathBuf::from("server-ui/package.json"),
        PathBuf::from("server-ui/pnpm-lock.yaml"),
        PathBuf::from("server-ui/postcss.config.js"),
        PathBuf::from("server-ui/tailwind.config.js"),
        PathBuf::from("server-ui/tsconfig.json"),
        PathBuf::from("server-ui/vite.config.ts"),
    ];
    collect_files(Path::new("server-ui/src"), &mut paths);
    paths.sort();
    paths
}

fn build_server_ui(out_dir: &Path) {
    if env::var_os("LNX_SKIP_SERVER_UI_BUILD").is_none() {
        if env::var_os("LNX_SKIP_SERVER_UI_INSTALL").is_none()
            && !Path::new("server-ui/node_modules/.modules.yaml").exists()
        {
            let status = Command::new("pnpm")
                .arg("--dir")
                .arg("server-ui")
                .arg("install")
                .arg("--frozen-lockfile")
                .status()
                .expect("run pnpm install for server UI");
            if !status.success() {
                panic!("server UI dependency install failed with {status}");
            }
        }
        let status = Command::new("pnpm")
            .arg("--dir")
            .arg("server-ui")
            .arg("build")
            .status()
            .expect("run pnpm build for server UI");
        if !status.success() {
            panic!("server UI build failed with {status}");
        }
    }
    let dist = Path::new("server-ui/dist");
    if !dist.join("index.html").exists() {
        panic!("server UI dist is missing; run pnpm --dir server-ui build");
    }
    let assets = out_dir.join("lnx_server_ui_assets.rs");
    fs::write(&assets, server_ui_asset_source(dist))
        .unwrap_or_else(|e| panic!("write embedded server UI assets {}: {e}", assets.display()));
}

fn server_ui_asset_source(dist: &Path) -> String {
    let mut files = Vec::new();
    collect_files(dist, &mut files);
    files.sort();

    let mut source = String::from(
        "pub(crate) struct ServerUiAsset {\n    pub(crate) path: &'static str,\n    pub(crate) mime: &'static str,\n    pub(crate) bytes: &'static [u8],\n}\n\npub(crate) static SERVER_UI_ASSETS: &[ServerUiAsset] = &[\n",
    );
    for file in files {
        let relative = file.strip_prefix(dist).expect("asset under dist");
        let web_path = format!("/{}", relative.to_string_lossy().replace('\\', "/"));
        let mime = mime_for_path(relative);
        source.push_str(&format!(
            "    ServerUiAsset {{ path: {:?}, mime: {:?}, bytes: include_bytes!({:?}) }},\n",
            web_path,
            mime,
            file.canonicalize().expect("canonicalize server UI asset")
        ));
    }
    source.push_str("];\n");
    source
}

fn mime_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
    {
        "css" => "text/css; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn source_stamp(paths: &[PathBuf]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for path in paths {
        hash_bytes(&mut hash, path.to_string_lossy().as_bytes());
        if let Ok(bytes) = fs::read(path) {
            hash_bytes(&mut hash, &bytes);
        }
    }
    hash
}

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x100000001b3);
    }
}
