use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=src/guest_agent.rs");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let agent = out_dir.join("lnx-agent");
    let target =
        env::var("LNX_AGENT_TARGET").unwrap_or_else(|_| "aarch64-unknown-linux-musl".to_string());
    let linker = env::var("LNX_AGENT_LINKER").unwrap_or_else(|_| "rust-lld".to_string());

    let status = Command::new("rustc")
        .args([
            "--edition=2024",
            "--target",
            &target,
            "-C",
            "opt-level=2",
            "-C",
            "target-feature=+crt-static",
            "-C",
            &format!("linker={linker}"),
        ])
        .arg("src/guest_agent.rs")
        .arg("-o")
        .arg(&agent)
        .status()
        .expect("run rustc for guest agent");
    if !status.success() {
        panic!("guest agent build failed with {status}");
    }

    println!("cargo:rustc-env=LNX_AGENT={}", agent.display());
}
