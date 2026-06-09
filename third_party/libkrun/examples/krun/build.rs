use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=guest_runner.rs");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let runner = out_dir.join("krun-command-runner");
    let status = Command::new("rustc")
        .args([
            "--edition=2024",
            "--target=aarch64-unknown-linux-musl",
            "-C",
            "opt-level=2",
            "-C",
            "target-feature=+crt-static",
            "-C",
            "linker=rust-lld",
        ])
        .arg("guest_runner.rs")
        .arg("-o")
        .arg(&runner)
        .status()
        .expect("failed to run rustc for guest runner");
    if !status.success() {
        panic!("guest runner rustc failed with {status}");
    }

    let fw_path = env::var_os("LIBKRUNFW_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../../../libkrunfw/libkrunfw.5.dylib"));
    println!("cargo:rerun-if-changed={}", fw_path.display());
    if !Path::new(&fw_path).exists() {
        panic!(
            "missing {}; set LIBKRUNFW_PATH to libkrunfw.5.dylib",
            fw_path.display()
        );
    }
    std::fs::copy(&fw_path, out_dir.join("libkrunfw.5.dylib"))
        .expect("copy libkrunfw.5.dylib into build output");
}
