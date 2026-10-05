//! The release assets lnx downloads by itself (the kernel, the base rootfs
//! and the nested Linux helper), pinned by the SHA-256 of each file as
//! downloaded. The pins are compiled into the binary from
//! `release_assets.json`, which `bun run images:pin <release>` writes, and a
//! download whose digest differs is refused before anything uses it.

use std::{collections::BTreeMap, fs, io::Read, path::Path, sync::OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Deserialize)]
struct Pins {
    release: String,
    sha256: BTreeMap<String, String>,
}

fn pins() -> &'static Pins {
    static PINS: OnceLock<Pins> = OnceLock::new();
    PINS.get_or_init(|| {
        serde_json::from_str(include_str!("release_assets.json"))
            .expect("release_assets.json is valid; a unit test checks it")
    })
}

/// The image release lnx downloads its kernel, rootfs and helpers from.
pub(crate) fn image_release() -> &'static str {
    &pins().release
}

/// The SHA-256 `asset` from `release` must have.
fn pinned_sha256(release: &str, asset: &str) -> Result<&'static str> {
    let pins = pins();
    if release != pins.release {
        bail!(
            "this lnx pins assets of {}, not {release}; refusing to download {asset} unverified",
            pins.release
        );
    }
    pins.sha256
        .get(asset)
        .map(String::as_str)
        .ok_or_else(|| anyhow!("this lnx has no SHA-256 pinned for {release}/{asset}"))
}

/// Checks a downloaded `release`/`asset` at `path` against its pinned digest.
pub(crate) fn verify_download(path: &Path, release: &str, asset: &str) -> Result<()> {
    let expected = pinned_sha256(release, asset)?;
    let actual = sha256_file(path)?;
    if actual != expected {
        bail!(
            "downloaded {release}/{asset} has SHA-256 {actual}, but this lnx expects {expected}; refusing to use it"
        );
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let read = file
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests;
