use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let tracked_paths = tracked_source_paths();
    for path in &tracked_paths {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-env-changed=LNX_AGENT_TARGET_DIR");
    println!("cargo:rerun-if-env-changed=LNX_AGENT_TARGET");
    println!("cargo:rerun-if-env-changed=LNX_AGENT_LINKER");

    let source_stamp = source_stamp(&tracked_paths);
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
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
