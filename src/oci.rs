use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};

use crate::{descriptor, init, paths::Layout};

/// Import an OCI image as an instance rootfs.
///
/// Layers are pulled host-side over the registry v2 protocol, but the
/// filesystem is assembled inside a private builder VM: only a Linux root
/// can preserve ownership, modes, and device nodes, and the guest kernel's
/// 16 KiB pages match the 16 KiB-block ext4 the managed rootfs layout
/// requires. The built image lands back on the host through the cwd share.
pub fn import_image(layout: &Layout, reference: &str, kernel: Option<&Path>) -> Result<()> {
    if layout.rootfs.exists() {
        bail!(
            "instance {} already has a rootfs: {}",
            layout.instance,
            layout.rootfs.display()
        );
    }
    match kernel {
        Some(kernel) => init::install_kernel(layout, kernel)?,
        None => init::ensure_kernel(layout)?,
    }

    let image = ImageReference::parse(reference)?;
    let staging = layout
        .base
        .join(format!("oci-import-{}", std::process::id()));
    let result = (|| {
        fs::create_dir_all(&staging).with_context(|| format!("create {}", staging.display()))?;
        let layers = pull_layers(&image, &staging)?;
        if layers.is_empty() {
            bail!("image has no layers");
        }
        let builder_instance = format!("{}-oci-builder", layout.instance);
        build_rootfs_with_instance(&staging, &builder_instance, Some(&layout.base))?;
        publish_rootfs(layout, &staging, reference)
    })();
    let _ = fs::remove_dir_all(&staging);
    result
}

struct ImageReference {
    registry: String,
    repository: String,
    reference: String,
}

impl ImageReference {
    /// Accepts [registry/]repository[:tag][@digest], docker-style: bare
    /// names default to docker.io/library and :latest.
    fn parse(input: &str) -> Result<Self> {
        let (rest, reference) = match input.split_once('@') {
            Some((rest, digest)) => (rest, digest.to_string()),
            None => match input.rsplit_once(':') {
                Some((rest, tag)) if !tag.contains('/') => (rest, tag.to_string()),
                _ => (input, "latest".to_string()),
            },
        };
        let mut parts = rest.splitn(2, '/');
        let first = parts.next().context("empty image reference")?;
        let (registry, repository) = match parts.next() {
            Some(remainder)
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (first.to_string(), remainder.to_string())
            }
            Some(remainder) => ("docker.io".to_string(), format!("{first}/{remainder}")),
            None => ("docker.io".to_string(), format!("library/{first}")),
        };
        Ok(Self {
            registry,
            repository,
            reference,
        })
    }

    fn registry_host(&self) -> &str {
        if self.registry == "docker.io" {
            "registry-1.docker.io"
        } else {
            &self.registry
        }
    }
}

fn pull_layers(image: &ImageReference, staging: &Path) -> Result<Vec<PathBuf>> {
    let token = fetch_token(image)?;
    let manifest = fetch_manifest(image, &token, &image.reference)?;
    // An image index / manifest list carries a `manifests` array; its
    // top-level mediaType is only recommended by the OCI spec and some
    // registries omit it, so detect the index by shape, not mediaType.
    let manifest = if manifest["manifests"].is_array() {
        let digest = manifest["manifests"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|entry| {
                entry["platform"]["os"] == "linux" && entry["platform"]["architecture"] == "arm64"
            })
            .and_then(|entry| entry["digest"].as_str())
            .with_context(|| format!("no linux/arm64 manifest for {}", image.repository))?
            .to_string();
        fetch_manifest(image, &token, &digest)?
    } else {
        manifest
    };

    let layers = manifest["layers"]
        .as_array()
        .context("manifest has no layers")?;
    let mut paths = Vec::new();
    for (index, layer) in layers.iter().enumerate() {
        let digest = layer["digest"].as_str().context("layer missing digest")?;
        let dest = staging.join(format!("layer-{index:03}"));
        eprintln!("oci: pull layer {} {digest}", index + 1);
        fetch_blob(image, &token, digest, &dest)?;
        verify_digest(&dest, digest)?;
        paths.push(dest);
    }
    Ok(paths)
}

fn fetch_token(image: &ImageReference) -> Result<Option<String>> {
    let probe = curl(&[
        "-sS",
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}\t%header{www-authenticate}",
        &format!("https://{}/v2/", image.registry_host()),
    ])?;
    let (status, challenge) = probe.split_once('\t').unwrap_or((probe.as_str(), ""));
    if status.trim() == "200" {
        return Ok(None);
    }
    let field = |key: &str| {
        challenge
            .split(&format!("{key}=\""))
            .nth(1)
            .and_then(|rest| rest.split('"').next().map(str::to_string))
    };
    let realm = field("realm").with_context(|| {
        format!(
            "registry {} sent no auth realm: {challenge}",
            image.registry
        )
    })?;
    let mut url = format!("{realm}?scope=repository:{}:pull", image.repository);
    if let Some(service) = field("service") {
        url.push_str(&format!("&service={service}"));
    }
    let body = curl(&["-fsS", &url]).context("fetch registry token")?;
    let token: serde_json::Value = serde_json::from_str(&body).context("parse token response")?;
    let token = token["token"]
        .as_str()
        .or_else(|| token["access_token"].as_str())
        .context("token response missing token")?
        .to_string();
    Ok(Some(token))
}

const MANIFEST_ACCEPT: &str = "Accept: application/vnd.oci.image.manifest.v1+json, \
application/vnd.oci.image.index.v1+json, \
application/vnd.docker.distribution.manifest.v2+json, \
application/vnd.docker.distribution.manifest.list.v2+json";

fn fetch_manifest(
    image: &ImageReference,
    token: &Option<String>,
    reference: &str,
) -> Result<serde_json::Value> {
    let url = format!(
        "https://{}/v2/{}/manifests/{reference}",
        image.registry_host(),
        image.repository
    );
    let mut args = vec![
        "-fsS".to_string(),
        "-H".to_string(),
        MANIFEST_ACCEPT.to_string(),
    ];
    if let Some(token) = token {
        args.push("-H".to_string());
        args.push(format!("Authorization: Bearer {token}"));
    }
    args.push(url.clone());
    let body = curl(&args.iter().map(String::as_str).collect::<Vec<_>>())
        .with_context(|| format!("fetch manifest {url}"))?;
    serde_json::from_str(&body).with_context(|| format!("parse manifest {url}"))
}

fn fetch_blob(
    image: &ImageReference,
    token: &Option<String>,
    digest: &str,
    dest: &Path,
) -> Result<()> {
    let url = format!(
        "https://{}/v2/{}/blobs/{digest}",
        image.registry_host(),
        image.repository
    );
    let mut args = vec!["-fsSL".to_string()];
    if let Some(token) = token {
        args.push("-H".to_string());
        args.push(format!("Authorization: Bearer {token}"));
    }
    args.push("-o".to_string());
    args.push(dest.to_string_lossy().into_owned());
    args.push(url.clone());
    curl(&args.iter().map(String::as_str).collect::<Vec<_>>())
        .with_context(|| format!("fetch blob {url}"))?;
    Ok(())
}

fn verify_digest(path: &Path, digest: &str) -> Result<()> {
    let hex = digest
        .strip_prefix("sha256:")
        .with_context(|| format!("unsupported digest {digest}"))?;
    let output = Command::new("shasum")
        .arg("-a")
        .arg("256")
        .arg(path)
        .output()
        .or_else(|_| Command::new("sha256sum").arg(path).output())
        .context("run sha256 tool")?;
    if !output.status.success() {
        bail!("sha256 of {} failed", path.display());
    }
    let actual = String::from_utf8_lossy(&output.stdout);
    let actual = actual.split_whitespace().next().unwrap_or_default();
    if actual != hex {
        bail!(
            "layer digest mismatch for {}: expected {hex}, got {actual}",
            path.display()
        );
    }
    Ok(())
}

fn curl(args: &[&str]) -> Result<String> {
    let output = Command::new("curl")
        .args(args)
        .output()
        .context("run curl")?;
    if !output.status.success() {
        bail!(
            "curl {:?} failed: {}",
            args.last().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Unpack the layers (whiteouts included) and build the 64 GiB 16 KiB-block
/// ext4 inside a private builder VM, writing the result into the staging
/// dir, which the builder sees as its cwd share.
const BUILD_SCRIPT: &str = r#"
set -eu
root=/var/tmp/lnx-oci-root
rm -rf "$root"
mkdir -p "$root"
for layer in ./layer-*; do
    # Apply whiteouts before extracting: a marker's basename starts with
    # ".wh." (".wh..wh..opq" makes the directory opaque). Match on the
    # basename so a real file merely containing ".wh." is left alone.
    tar -tf "$layer" | while IFS= read -r entry; do
        name="$(basename "$entry")"
        case "$name" in
            .wh..wh..opq)
                dir="$(dirname "$entry")"
                find "$root/$dir" -mindepth 1 -maxdepth 1 -exec rm -rf {} + 2>/dev/null || true
                ;;
            .wh.*)
                dir="$(dirname "$entry")"
                rm -rf "$root/$dir/${name#.wh.}"
                ;;
        esac
    done
    tar -xpf "$layer" -C "$root" --numeric-owner
    # Drop the extracted marker files themselves (basename starts with .wh.).
    find "$root" -depth -name '.wh.*' -exec rm -rf {} + 2>/dev/null || true
done
rm -f ./rootfs.ext4
truncate -s 64G ./rootfs.ext4
mkfs.ext4 -F -q -b 16384 -d "$root" ./rootfs.ext4
rm -rf "$root"
echo BUILD_OK
"#;

pub fn build_rootfs(staging: &Path) -> Result<()> {
    let builder_instance = std::env::var("LNX_OCI_BUILDER_INSTANCE")
        .unwrap_or_else(|_| format!("oci-builder-{}", std::process::id()));
    build_rootfs_with_instance(staging, &builder_instance, None)
}

fn build_rootfs_with_instance(
    staging: &Path,
    builder_instance: &str,
    base: Option<&Path>,
) -> Result<()> {
    eprintln!("oci: building rootfs in builder VM {builder_instance}");
    let exe = std::env::current_exe().context("current executable")?;
    let builder_layout = match base {
        Some(base) => Layout::resolve_in_base(builder_instance, base.to_path_buf(), None, None),
        None => Layout::resolve(builder_instance, None, None)
            .with_context(|| format!("resolve builder instance {builder_instance}"))?,
    };
    cleanup_builder_instance(&builder_layout);
    // Only root can preserve ownership and device nodes while unpacking.
    let mut command = Command::new(exe);
    command.arg("--instance").arg(builder_instance);
    command.env(crate::packages::SKIP_DEFAULT_PACKAGES_ENV, "1");
    if let Some(base) = base {
        command.env("LNX_BASE", base);
    }
    let output = command
        .arg("--root")
        .arg("bash")
        .arg("-lc")
        .arg(BUILD_SCRIPT)
        .current_dir(staging)
        .output()
        .context("run builder VM")?;
    cleanup_builder_instance(&builder_layout);
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || !stdout.contains("BUILD_OK") {
        bail!(
            "rootfs build failed ({}):\n{}\n{}",
            output.status,
            stdout.trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let built = staging.join("rootfs.ext4");
    if !built.exists() {
        bail!(
            "rootfs build completed but {} is not visible on the host; use a staging directory that is not isolated by host-share copy-on-write",
            built.display()
        );
    }
    Ok(())
}

fn cleanup_builder_instance(layout: &Layout) {
    let _ = fs::remove_dir_all(&layout.run_dir);
    if layout.run_dir != layout.instance_dir {
        let _ = fs::remove_dir_all(&layout.instance_dir);
    }
}

fn publish_rootfs(layout: &Layout, staging: &Path, reference: &str) -> Result<()> {
    let built = staging.join("rootfs.ext4");
    init::validate_managed_rootfs_at(&built)?;
    if let Some(parent) = layout.rootfs.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::rename(&built, &layout.rootfs)
        .with_context(|| format!("move {} to {}", built.display(), layout.rootfs.display()))?;
    descriptor::ensure_identity(layout, &format!("oci:{reference}"))?;
    eprintln!("oci: imported {reference} as {}", layout.rootfs.display());
    Ok(())
}

#[cfg(test)]
mod tests;
