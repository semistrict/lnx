use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=src/guest_agent.rs");
    println!("cargo:rerun-if-changed=src/protocol.rs");
    println!("cargo:rerun-if-changed=guest-agent/Cargo.toml");
    println!("cargo:rerun-if-changed=guest-agent/src/main.rs");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let agent = out_dir.join("lnx-agent");
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
        .env("RUSTFLAGS", "-C target-feature=+crt-static");
    let status = command.status().expect("run cargo for guest agent");
    if !status.success() {
        panic!("guest agent build failed with {status}");
    }
    let built = PathBuf::from("guest-agent")
        .join("target")
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
}
