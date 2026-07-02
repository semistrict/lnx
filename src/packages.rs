#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use lnx_protocol::PACKAGE_PROFILE_NAME;
use serde::{Deserialize, Serialize};

use crate::{descriptor, init, paths::Layout, runner};

const STORE_IMAGE: &str = "store.sparsebundle";
const STORE_MOUNT: &str = "mount";
const MANIFEST: &str = "manifest.json";
const CLOSURE_FILE: &str = "closure";
#[cfg(target_os = "macos")]
const DEFAULT_STORE_IMAGE_SIZE: &str = "32g";

pub const DEFAULT_BUILDER_INSTANCE: &str = "nix-builder";
pub const DEFAULT_BUILDER_IMAGE: &str = "nixos/nix:latest";
pub const DEFAULT_BINARIES: &[&str] = &["node", "npm", "npx", "pnpm"];
pub const DEFAULT_PACKAGES: &[&str] = &[
    "github:NixOS/nixpkgs/nixos-unstable#nodejs_latest",
    "github:NixOS/nixpkgs/nixos-unstable#pnpm",
];

pub const SKIP_DEFAULT_PACKAGES_ENV: &str = "LNX_SKIP_DEFAULT_PACKAGES";

/// Stamp value recorded in shares.stamp when no package store is mounted.
pub const STAMP_DISABLED: &str = "disabled-v1";

fn store_root_dir() -> String {
    format!("stores/nix-linux-{}", std::env::consts::ARCH)
}

/// The base directory the shared package store lives under: LNX_BASE when
/// set, otherwise ~/.lnx. The store is shared across all instance bases so
/// project-local .lnx directories do not each grow their own multi-gigabyte
/// store image and builder instance.
pub fn global_base() -> Result<PathBuf> {
    if let Some(base) = std::env::var_os("LNX_BASE") {
        return Ok(PathBuf::from(base));
    }
    Ok(dirs::home_dir()
        .context("could not resolve home directory")?
        .join(".lnx"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum GuestStoreMode {
    /// Mount the shared package store read-only when it exists
    Auto,
    /// Never mount the shared package store
    Disabled,
    /// Mount the shared package store writable (package installs)
    Writable,
}

impl GuestStoreMode {
    pub fn as_arg(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Disabled => "disabled",
            Self::Writable => "writable",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreLayout {
    pub root: PathBuf,
    pub image: PathBuf,
    pub mount: PathBuf,
    pub store: PathBuf,
    pub var: PathBuf,
    pub profiles: PathBuf,
    pub manifest: PathBuf,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct PackageManifest {
    pub packages: Vec<String>,
    pub binaries: Vec<String>,
    /// Store path basenames of the profile closure, as resolved by the last
    /// install. `packages gc` keeps exactly these.
    #[serde(default)]
    pub closure: Vec<String>,
    /// Guest-absolute store path the profile pointed at after the last
    /// install, like /nix/store/<hash>-profile.
    #[serde(default)]
    pub profile_target: Option<String>,
}

impl StoreLayout {
    pub fn resolve(base: &Path) -> Self {
        let root = base.join(store_root_dir());
        let mount = root.join(STORE_MOUNT);
        Self {
            image: root.join(STORE_IMAGE),
            store: mount.join("store"),
            var: mount.join("var").join("nix"),
            profiles: mount.join("profiles"),
            manifest: root.join(MANIFEST),
            mount,
            root,
        }
    }

    pub fn resolve_global() -> Result<Self> {
        Ok(Self::resolve(&global_base()?))
    }

    pub fn profile_link(&self) -> PathBuf {
        self.profiles.join(PACKAGE_PROFILE_NAME)
    }

    fn closure_file(&self) -> PathBuf {
        self.mount.join(CLOSURE_FILE)
    }

    /// Stamp value recorded in shares.stamp for a mounted store. Bump the
    /// version when the guest-visible mount topology changes: a restored
    /// snapshot keeps its snapshot-time virtiofs devices and mounts.
    pub fn stamp_value(&self, writable: bool) -> String {
        let mode = if writable {
            "writable-v1"
        } else {
            "readonly-v2"
        };
        format!("{mode} root={}", self.root.display())
    }

    pub fn is_ready(&self) -> bool {
        self.store.is_dir()
            && fs::symlink_metadata(self.profile_link()).is_ok()
            && self
                .resolve_profile_path("bin")
                .is_some_and(|path| path.is_dir())
    }

    pub fn manifest_is_coherent(&self, manifest: &PackageManifest) -> bool {
        manifest
            .binaries
            .iter()
            .all(|binary| self.profile_binary_exists(binary))
    }

    pub fn profile_binary_exists(&self, binary: &str) -> bool {
        self.resolve_profile_path(&format!("bin/{binary}"))
            .is_some_and(|path| path.is_file())
    }

    fn resolve_profile_path(&self, suffix: &str) -> Option<PathBuf> {
        let profile = self.resolve_store_link(&self.profile_link())?;
        let mut path = profile;
        for part in suffix.split('/').filter(|part| !part.is_empty()) {
            path.push(part);
            path = self.resolve_store_link(&path)?;
        }
        Some(path)
    }

    fn resolve_store_link(&self, path: &Path) -> Option<PathBuf> {
        let link = match fs::read_link(path) {
            Ok(link) => link,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
                return Some(path.to_path_buf());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) => return Some(path.to_path_buf()),
        };
        if let Ok(store_relative) = link.strip_prefix("/nix/store") {
            return Some(self.store.join(store_relative));
        }
        if link.is_absolute() {
            Some(link)
        } else {
            path.parent().map(|parent| parent.join(link))
        }
    }

    pub fn ensure(&self) -> Result<()> {
        self.ensure_storage()?;
        for path in [&self.store, &self.var, &self.profiles] {
            fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
        }
        Ok(())
    }

    pub fn prepare_readonly(&self) -> Result<bool> {
        if !self.storage_exists() {
            return Ok(false);
        }
        self.ensure_attached()?;
        Ok(self.is_ready())
    }

    pub fn write_manifest(&self, manifest: &PackageManifest) -> Result<()> {
        self.ensure()?;
        let data = serde_json::to_string_pretty(manifest).context("encode package manifest")?;
        fs::write(&self.manifest, data + "\n")
            .with_context(|| format!("write {}", self.manifest.display()))
    }

    pub fn read_manifest(&self) -> Result<Option<PackageManifest>> {
        let data = match fs::read_to_string(&self.manifest) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("read {}", self.manifest.display())),
        };
        serde_json::from_str(&data)
            .with_context(|| format!("parse {}", self.manifest.display()))
            .map(Some)
    }

    fn read_closure_file(&self) -> Result<Vec<String>> {
        let path = self.closure_file();
        let data = fs::read_to_string(&path)
            .with_context(|| format!("read install closure {}", path.display()))?;
        Ok(data
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect())
    }

    fn storage_exists(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.image.exists() || self.store.is_dir()
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.mount.exists()
        }
    }

    /// A store without an image file is backed by a plain directory: always
    /// the case on Linux hosts, and used by tests on macOS.
    #[cfg(target_os = "macos")]
    fn directory_backed(&self) -> bool {
        !self.image.exists() && self.store.is_dir()
    }

    fn ensure_storage(&self) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("create {}", self.root.display()))?;
        #[cfg(target_os = "macos")]
        {
            if !self.image.exists() && !self.directory_backed() {
                create_case_sensitive_apfs_image(&self.image)?;
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            fs::create_dir_all(&self.mount)
                .with_context(|| format!("create {}", self.mount.display()))?;
        }
        self.ensure_attached()
    }

    fn ensure_attached(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            fs::create_dir_all(&self.mount)
                .with_context(|| format!("create {}", self.mount.display()))?;
            if self.directory_backed() || is_mountpoint(&self.mount)? {
                return Ok(());
            }
            let status = Command::new("hdiutil")
                .arg("attach")
                .arg(&self.image)
                .arg("-mountpoint")
                .arg(&self.mount)
                .arg("-nobrowse")
                .arg("-quiet")
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .with_context(|| format!("attach {}", self.image.display()))?;
            if !status.success() {
                anyhow::bail!("hdiutil attach failed for {}", self.image.display());
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            fs::create_dir_all(&self.mount)
                .with_context(|| format!("create {}", self.mount.display()))?;
        }
        Ok(())
    }

    /// Detach the store image. Fails while any process (including a running
    /// VM's virtiofs backend) still uses the mount, which is exactly the
    /// guard gc needs.
    #[cfg(target_os = "macos")]
    fn detach(&self) -> Result<()> {
        if self.directory_backed() || !is_mountpoint(&self.mount)? {
            return Ok(());
        }
        let status = Command::new("hdiutil")
            .arg("detach")
            .arg(&self.mount)
            .arg("-quiet")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| format!("detach {}", self.mount.display()))?;
        if !status.success() {
            anyhow::bail!(
                "hdiutil detach failed for {} (is an instance still running?)",
                self.mount.display()
            );
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn create_case_sensitive_apfs_image(image: &Path) -> Result<()> {
    if let Some(parent) = image.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let status = Command::new("hdiutil")
        .arg("create")
        .arg(image)
        .arg("-type")
        .arg("SPARSEBUNDLE")
        .arg("-fs")
        .arg("Case-sensitive APFS")
        .arg("-volname")
        .arg("lnx-nix-linux")
        .arg("-size")
        .arg(DEFAULT_STORE_IMAGE_SIZE)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("create {}", image.display()))?;
    if !status.success() {
        anyhow::bail!("hdiutil create failed for {}", image.display());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn is_mountpoint(path: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
    };
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent", path.display()))?;
    let parent_metadata =
        fs::metadata(parent).with_context(|| format!("stat {}", parent.display()))?;
    Ok(metadata.dev() != parent_metadata.dev())
}

pub fn default_packages() -> Vec<String> {
    DEFAULT_PACKAGES
        .iter()
        .map(|package| (*package).to_string())
        .collect()
}

pub fn default_binaries() -> Vec<String> {
    DEFAULT_BINARIES
        .iter()
        .map(|binary| (*binary).to_string())
        .collect()
}

/// A bare name like `ripgrep` means `nixpkgs#ripgrep`; anything already
/// carrying a flake ref (`#`) or URL scheme (`:`) passes through.
pub fn normalize_package_ref(package: &str) -> String {
    if package.contains('#') || package.contains(':') {
        package.to_string()
    } else {
        format!("nixpkgs#{package}")
    }
}

pub struct InstallRequest {
    pub builder: String,
    pub builder_image: String,
    pub binaries: Vec<String>,
    pub packages: Vec<String>,
}

impl Default for InstallRequest {
    fn default() -> Self {
        Self {
            builder: DEFAULT_BUILDER_INSTANCE.to_string(),
            builder_image: DEFAULT_BUILDER_IMAGE.to_string(),
            binaries: Vec::new(),
            packages: Vec::new(),
        }
    }
}

pub fn install(request: InstallRequest, cpus: u8, memory_mib: u32) -> Result<PackageManifest> {
    let installing_defaults = request.packages.is_empty();
    let requested_packages = if installing_defaults {
        default_packages()
    } else {
        request
            .packages
            .iter()
            .map(|package| normalize_package_ref(package))
            .collect()
    };
    let requested_binaries = if request.binaries.is_empty() && installing_defaults {
        default_binaries()
    } else {
        request.binaries
    };
    validate_binaries(&requested_binaries)?;

    let base = global_base()?;
    let store = StoreLayout::resolve(&base);
    let store_ready = store.prepare_readonly()?;
    let existing = store.read_manifest()?;
    let use_existing = existing
        .as_ref()
        .is_some_and(|manifest| store_ready && store.manifest_is_coherent(manifest));
    let (mut package_refs, mut binaries) = if use_existing {
        let manifest = existing.as_ref().expect("checked above");
        (manifest.packages.clone(), manifest.binaries.clone())
    } else {
        (Vec::new(), Vec::new())
    };
    append_unique(&mut package_refs, requested_packages);
    append_unique(&mut binaries, requested_binaries);
    if package_refs.is_empty() {
        package_refs = default_packages();
    }
    validate_binaries(&binaries)?;

    let builder_layout = Layout::resolve_in_base(&request.builder, base, None, None);
    ensure_builder_instance(&builder_layout, &request.builder, &request.builder_image)?;
    let cwd = std::env::current_dir().context("current directory")?;
    let status = runner::run(runner::RunConfig {
        layout: builder_layout,
        command: vec![
            "sh".to_string(),
            "-lc".to_string(),
            install_script(&package_refs, &binaries),
        ],
        cwd,
        cpus,
        memory_mib,
        nested_kvm: false,
        restore_snapshot: None,
        forwards: Vec::new(),
        snapshot_output: None,
        run_as_root: true,
        no_host_shares: true,
        package_store: GuestStoreMode::Writable,
        vhost_user_fs: Vec::new(),
        reuse_owner: false,
        deterministic: None,
        trace_events: false,
    })?;
    if status != 0 {
        bail!("package install exited with status {status}");
    }

    let closure = store.read_closure_file()?;
    let profile_target = fs::read_link(store.profile_link())
        .with_context(|| format!("read profile link {}", store.profile_link().display()))?
        .to_string_lossy()
        .into_owned();
    let manifest = PackageManifest {
        packages: package_refs,
        binaries,
        closure,
        profile_target: Some(profile_target),
    };
    store.write_manifest(&manifest)?;
    Ok(manifest)
}

/// Install the default packages on first run. Skipped for internal builder
/// instances, when host shares or the package store are off, and under
/// LNX_SKIP_DEFAULT_PACKAGES.
pub fn ensure_default_store(
    instance: &str,
    cpus: u8,
    memory_mib: u32,
    no_host_shares: bool,
    mode: GuestStoreMode,
) -> Result<()> {
    if !should_bootstrap_default_store(
        instance,
        no_host_shares,
        std::env::var_os(SKIP_DEFAULT_PACKAGES_ENV).is_some(),
        mode,
    ) {
        return Ok(());
    }

    let store = StoreLayout::resolve_global()?;
    if store.prepare_readonly()? {
        return Ok(());
    }

    eprintln!("first run: package store missing, installing default packages");
    install(InstallRequest::default(), cpus, memory_mib)
        .context("install default packages")
        .map(|_| ())
}

pub fn should_bootstrap_default_store(
    instance: &str,
    no_host_shares: bool,
    skip_env: bool,
    mode: GuestStoreMode,
) -> bool {
    !no_host_shares
        && !skip_env
        && !matches!(mode, GuestStoreMode::Disabled)
        && !instance.ends_with("-oci-builder")
}

fn ensure_builder_instance(layout: &Layout, builder: &str, builder_image: &str) -> Result<()> {
    let expected_image = format!("oci:{builder_image}");
    let descriptor = descriptor::load(layout)?;
    if layout.rootfs.exists() && descriptor.image.as_deref() == Some(expected_image.as_str()) {
        init::ensure_kernel(layout)?;
        init::ensure_instance(layout)?;
        return Ok(());
    }

    if layout.rootfs.exists() || layout.instance_dir.exists() {
        if builder != DEFAULT_BUILDER_INSTANCE {
            bail!(
                "builder instance {builder} already exists and is not image {builder_image}; choose another --builder or remove it"
            );
        }
        let _ = fs::remove_dir_all(&layout.run_dir);
        if layout.run_dir != layout.instance_dir {
            let _ = fs::remove_dir_all(&layout.instance_dir);
        }
    }

    crate::oci::import_image(layout, builder_image, None)
        .with_context(|| format!("initialize package builder from {builder_image}"))
}

fn validate_binaries(binaries: &[String]) -> Result<()> {
    for binary in binaries {
        if binary.is_empty()
            || !binary
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            bail!("invalid package binary name {binary:?}");
        }
    }
    Ok(())
}

fn append_unique(values: &mut Vec<String>, new_values: Vec<String>) {
    for value in new_values {
        if !values.iter().any(|existing| existing == &value) {
            values.push(value);
        }
    }
}

#[derive(Debug, Default)]
pub struct GcOutcome {
    pub removed: usize,
    pub removed_bytes: u64,
    pub kept: usize,
}

/// Remove store paths not in the manifest closure. Host-side only: the
/// closure was resolved by nix at install time, so no nix is needed here.
pub fn gc() -> Result<GcOutcome> {
    let store = StoreLayout::resolve_global()?;
    if !store.prepare_readonly()? {
        bail!("package store is not initialized; nothing to collect");
    }
    let manifest = store
        .read_manifest()?
        .filter(|manifest| !manifest.closure.is_empty())
        .context(
            "package manifest records no closure (predates gc support); \
             run `lnx packages install` once to record it",
        )?;

    let mut keep: BTreeSet<String> = manifest.closure.iter().cloned().collect();
    if let Some(target) = &manifest.profile_target
        && let Some(name) = Path::new(target).file_name()
    {
        keep.insert(name.to_string_lossy().into_owned());
    }

    // Refuse to collect while an instance still uses the store: on macOS a
    // detach fails while the virtiofs backend holds the mount open.
    #[cfg(target_os = "macos")]
    {
        store.detach()?;
        store.ensure_attached()?;
    }

    let mut outcome = GcOutcome::default();
    for entry in
        fs::read_dir(&store.store).with_context(|| format!("read {}", store.store.display()))?
    {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if keep.contains(&name) {
            outcome.kept += 1;
            continue;
        }
        let path = entry.path();
        outcome.removed_bytes += path_size(&path);
        remove_path(&path)?;
        outcome.removed += 1;
    }

    #[cfg(target_os = "macos")]
    if store.image.exists() {
        store.detach()?;
        let status = Command::new("hdiutil")
            .arg("compact")
            .arg(&store.image)
            .arg("-quiet")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| format!("compact {}", store.image.display()))?;
        if !status.success() {
            anyhow::bail!("hdiutil compact failed for {}", store.image.display());
        }
    }

    Ok(outcome)
}

fn path_size(path: &Path) -> u64 {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !metadata.is_dir() {
        return metadata.len();
    }
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| path_size(&entry.path()))
        .sum()
}

fn remove_path(path: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if metadata.is_dir() {
        fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))
    } else {
        fs::remove_file(path).with_context(|| format!("remove {}", path.display()))
    }
}

pub fn install_script(packages: &[String], binaries: &[String]) -> String {
    let packages = shell_words(packages);
    let binary_checks = binaries
        .iter()
        .map(|binary| {
            let path = format!("\"$profile/bin/{binary}\"");
            format!(
                r#"if [ ! -x {path} ]; then
  echo "package profile does not provide executable: {binary}" >&2
  exit 127
fi
"#
            )
        })
        .collect::<String>();
    format!(
        r#"set -eu
export PATH="/nix/var/nix/profiles/default/bin:/root/.nix-profile/bin:$PATH"
export NIX_CONFIG="$(printf 'experimental-features = nix-command flakes\nbuild-users-group =\n')"
out=/run/lnx/nix
profile=/tmp/lnx-package-profile
mkdir -p "$out/store" "$out/profiles"
rm -rf "$profile" "$profile"-*
if ! command -v nix >/dev/null 2>&1; then
  echo "package builder image does not provide nix" >&2
  exit 125
fi
nix --extra-experimental-features 'nix-command flakes' profile install --refresh --profile "$profile" {packages}
{binary_checks}closure="$out/.closure.$$"
: > "$closure"
nix --extra-experimental-features 'nix-command flakes' path-info -r "$profile" | while IFS= read -r path; do
  name="${{path##*/}}"
  printf '%s\n' "$name" >> "$closure"
  dest="$out/store/$name"
  if [ -e "$dest" ]; then
    continue
  fi
  tmp="$out/store/.copy-$name-$$"
  rm -rf "$tmp"
  cp -R --no-preserve=mode,ownership,timestamps "$path" "$tmp"
  mv "$tmp" "$dest"
done
mv -f "$closure" "$out/{CLOSURE_FILE}"
target="$(readlink -f "$profile")"
case "$target" in
  /nix/store/*) ;;
  *) echo "unexpected package profile target: $target" >&2; exit 1 ;;
esac
tmp_link="$out/profiles/.{PACKAGE_PROFILE_NAME}.$$"
rm -f "$tmp_link"
ln -s "$target" "$tmp_link"
mv -Tf "$tmp_link" "$out/profiles/{PACKAGE_PROFILE_NAME}"
"#
    )
}

fn shell_words(words: &[String]) -> String {
    words
        .iter()
        .map(|word| shell_quote(word))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arch_store_root(base: &str) -> String {
        format!("{base}/stores/nix-linux-{}", std::env::consts::ARCH)
    }

    #[test]
    fn store_layout_uses_shared_store_root() {
        let layout = StoreLayout::resolve(Path::new("/tmp/base"));
        let root = arch_store_root("/tmp/base");
        assert_eq!(
            layout.image,
            PathBuf::from(format!("{root}/store.sparsebundle"))
        );
        assert_eq!(layout.mount, PathBuf::from(format!("{root}/mount")));
        assert_eq!(layout.store, PathBuf::from(format!("{root}/mount/store")));
        assert_eq!(layout.var, PathBuf::from(format!("{root}/mount/var/nix")));
        assert_eq!(
            layout.profile_link(),
            PathBuf::from(format!("{root}/mount/profiles/default"))
        );
    }

    #[test]
    fn stamp_value_reflects_mode_and_root() {
        let layout = StoreLayout::resolve(Path::new("/tmp/base"));
        let root = arch_store_root("/tmp/base");
        assert_eq!(
            layout.stamp_value(false),
            format!("readonly-v2 root={root}")
        );
        assert_eq!(layout.stamp_value(true), format!("writable-v1 root={root}"));
    }

    #[test]
    fn normalize_package_ref_defaults_bare_names_to_nixpkgs() {
        assert_eq!(normalize_package_ref("ripgrep"), "nixpkgs#ripgrep");
        assert_eq!(normalize_package_ref("nixpkgs#go"), "nixpkgs#go");
        assert_eq!(
            normalize_package_ref("github:NixOS/nixpkgs/nixos-unstable#pnpm"),
            "github:NixOS/nixpkgs/nixos-unstable#pnpm"
        );
    }

    #[test]
    fn bootstrap_skips_internal_builders_disabled_mode_and_env() {
        assert!(should_bootstrap_default_store(
            "default",
            false,
            false,
            GuestStoreMode::Auto
        ));
        assert!(!should_bootstrap_default_store(
            "default",
            true,
            false,
            GuestStoreMode::Auto
        ));
        assert!(!should_bootstrap_default_store(
            "default",
            false,
            true,
            GuestStoreMode::Auto
        ));
        assert!(!should_bootstrap_default_store(
            "default",
            false,
            false,
            GuestStoreMode::Disabled
        ));
        assert!(!should_bootstrap_default_store(
            "nix-builder-oci-builder",
            false,
            false,
            GuestStoreMode::Auto
        ));
    }

    #[test]
    fn is_ready_accepts_guest_absolute_profile_link() {
        let temp = tempfile::tempdir().unwrap();
        let layout = StoreLayout::resolve(temp.path());
        fs::create_dir_all(layout.store.join("example-profile/bin")).unwrap();
        fs::create_dir_all(&layout.profiles).unwrap();
        std::os::unix::fs::symlink("/nix/store/example-profile", layout.profile_link()).unwrap();

        assert!(layout.is_ready());
    }

    #[test]
    fn manifest_coherence_resolves_guest_store_symlinks_on_host() {
        let temp = tempfile::tempdir().unwrap();
        let layout = StoreLayout::resolve(temp.path());
        fs::create_dir_all(layout.store.join("go-1/bin")).unwrap();
        fs::write(layout.store.join("go-1/bin/go"), b"").unwrap();
        fs::create_dir_all(layout.store.join("profile")).unwrap();
        std::os::unix::fs::symlink("/nix/store/go-1/bin", layout.store.join("profile/bin"))
            .unwrap();
        fs::create_dir_all(&layout.profiles).unwrap();
        std::os::unix::fs::symlink("/nix/store/profile", layout.profile_link()).unwrap();

        assert!(layout.manifest_is_coherent(&PackageManifest {
            packages: vec!["nixpkgs#go".to_string()],
            binaries: vec!["go".to_string()],
            ..PackageManifest::default()
        }));
        assert!(!layout.manifest_is_coherent(&PackageManifest {
            packages: vec!["nixpkgs#go".to_string()],
            binaries: vec!["node".to_string()],
            ..PackageManifest::default()
        }));
    }

    #[test]
    fn manifest_round_trips_closure_and_accepts_legacy_json() {
        let temp = tempfile::tempdir().unwrap();
        let layout = StoreLayout::resolve(temp.path());
        fs::create_dir_all(&layout.store).unwrap();
        let manifest = PackageManifest {
            packages: vec!["nixpkgs#go".to_string()],
            binaries: vec!["go".to_string()],
            closure: vec!["abc-go-1.22".to_string(), "def-glibc-2.39".to_string()],
            profile_target: Some("/nix/store/xyz-profile".to_string()),
        };
        layout.write_manifest(&manifest).unwrap();
        assert_eq!(layout.read_manifest().unwrap(), Some(manifest));

        fs::write(
            &layout.manifest,
            r#"{"packages":["nixpkgs#go"],"binaries":["go"]}"#,
        )
        .unwrap();
        let legacy = layout.read_manifest().unwrap().unwrap();
        assert_eq!(legacy.closure, Vec::<String>::new());
        assert_eq!(legacy.profile_target, None);
    }

    #[test]
    fn install_script_quotes_packages_and_records_closure() {
        let script = install_script(
            &["nixpkgs#nodejs".to_string(), "weird'pkg".to_string()],
            &["node".to_string()],
        );
        assert!(script.contains("'nixpkgs#nodejs'"));
        assert!(script.contains("'weird'\\''pkg'"));
        assert!(script.contains("--profile \"$profile\""));
        assert!(script.contains("readlink -f \"$profile\""));
        assert!(script.contains("\"$profile/bin/node\""));
        assert!(script.contains("package profile does not provide executable: node"));
        assert!(script.contains("printf '%s\\n' \"$name\" >> \"$closure\""));
        assert!(script.contains("mv -f \"$closure\" \"$out/closure\""));
        assert!(script.contains("mv -Tf \"$tmp_link\" \"$out/profiles/default\""));
        assert!(!script.contains("--version"));
        assert!(!script.contains("apt-get"));
        assert!(!script.contains("nixos.org/nix/install"));
        assert!(!script.contains("cp -a"));
    }

    #[test]
    fn install_script_without_binaries_has_no_checks() {
        let script = install_script(&["nixpkgs#ripgrep".to_string()], &[]);
        assert!(!script.contains("does not provide executable"));
    }

    #[test]
    fn gc_keeps_closure_and_profile_target() {
        let temp = tempfile::tempdir().unwrap();
        // gc() resolves the global store; point it at the temp base. Safe
        // under nextest's process-per-test model.
        unsafe { std::env::set_var("LNX_BASE", temp.path()) };
        let layout = StoreLayout::resolve(temp.path());
        fs::create_dir_all(layout.store.join("keep-1/bin")).unwrap();
        fs::write(layout.store.join("keep-1/bin/go"), b"binary").unwrap();
        fs::create_dir_all(layout.store.join("stale-1")).unwrap();
        fs::write(layout.store.join("stale-1/data"), b"stale bytes").unwrap();
        fs::write(layout.store.join(".copy-stale-2-99"), b"tmp").unwrap();
        fs::create_dir_all(layout.store.join("profile-1")).unwrap();
        fs::create_dir_all(&layout.profiles).unwrap();
        std::os::unix::fs::symlink("/nix/store/keep-1/bin", layout.profiles.join("x")).unwrap();
        std::os::unix::fs::symlink("/nix/store/profile-1", layout.profile_link()).unwrap();
        fs::create_dir_all(layout.store.join("profile-1/bin")).unwrap();
        layout
            .write_manifest(&PackageManifest {
                packages: vec!["nixpkgs#go".to_string()],
                binaries: Vec::new(),
                closure: vec!["keep-1".to_string(), "profile-1".to_string()],
                profile_target: Some("/nix/store/profile-1".to_string()),
            })
            .unwrap();

        let outcome = gc().unwrap();

        assert_eq!(outcome.removed, 2);
        assert_eq!(outcome.kept, 2);
        assert!(outcome.removed_bytes >= 14);
        assert!(layout.store.join("keep-1").is_dir());
        assert!(layout.store.join("profile-1").is_dir());
        assert!(!layout.store.join("stale-1").exists());
        assert!(!layout.store.join(".copy-stale-2-99").exists());
    }
}
