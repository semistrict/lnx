fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    if target_os == "macos" && target_arch == "aarch64" {
        cc::Build::new()
            .file("src/hvf_simd_shim.c")
            .compile("krun_hvf_simd_shim");
    }
}
