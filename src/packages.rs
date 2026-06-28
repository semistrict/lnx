#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const STORE_ROOT: &str = "stores/nix-linux-aarch64";
const STORE_IMAGE: &str = "store.sparsebundle";
const STORE_MOUNT: &str = "mount";
const MANIFEST: &str = "manifest.json";
const PROFILE_NAME: &str = "default";
#[cfg(target_os = "macos")]
const DEFAULT_STORE_IMAGE_SIZE: &str = "32g";

pub const DEFAULT_BUILDER_INSTANCE: &str = "nix-builder";
pub const DEFAULT_BUILDER_IMAGE: &str = "nixos/nix:latest";
pub const DEFAULT_BINARIES: &[&str] = &["node", "npm", "npx", "pnpm"];
pub const DEFAULT_PACKAGES: &[&str] = &[
    "github:NixOS/nixpkgs/nixos-unstable#nodejs_latest",
    "github:NixOS/nixpkgs/nixos-unstable#pnpm",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestStoreMode {
    Auto,
    Disabled,
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

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "disabled" => Some(Self::Disabled),
            "writable" => Some(Self::Writable),
            _ => None,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PackageManifest {
    pub packages: Vec<String>,
    pub binaries: Vec<String>,
}

impl StoreLayout {
    pub fn resolve(base: &Path) -> Self {
        let root = base.join(STORE_ROOT);
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

    pub fn profile_link(&self) -> PathBuf {
        self.profiles.join(PROFILE_NAME)
    }

    pub fn is_ready(&self) -> bool {
        self.store.is_dir() && fs::symlink_metadata(self.profile_link()).is_ok()
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

    pub fn binaries(&self) -> Result<Vec<String>> {
        Ok(self
            .read_manifest()?
            .map(|manifest| manifest.binaries)
            .unwrap_or_else(default_binaries))
    }

    fn storage_exists(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.image.exists()
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.mount.exists()
        }
    }

    fn ensure_storage(&self) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("create {}", self.root.display()))?;
        #[cfg(target_os = "macos")]
        {
            if !self.image.exists() {
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
            if is_mountpoint(&self.mount)? {
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
        .arg("lnx-nix-linux-aarch64")
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

pub fn install_script(packages: &[String], binaries: &[String]) -> String {
    let packages = shell_words(packages);
    let version_checks = binaries
        .iter()
        .map(|binary| {
            let path = shell_quote(&format!("/run/lnx/nix/profiles/default/bin/{binary}"));
            format!("{path} --version\n")
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
nix --extra-experimental-features 'nix-command flakes' path-info -r "$profile" | while IFS= read -r path; do
  name="${{path##*/}}"
  dest="$out/store/$name"
  if [ -e "$dest" ]; then
    continue
  fi
  tmp="$out/store/.copy-$name-$$"
  rm -rf "$tmp"
  cp -R --no-preserve=mode,ownership,timestamps "$path" "$tmp"
  mv "$tmp" "$dest"
done
target="$(readlink -f "$profile")"
case "$target" in
  /nix/store/*) ;;
  *) echo "unexpected package profile target: $target" >&2; exit 1 ;;
esac
ln -sfn "$target" "$out/profiles/default"
{version_checks}
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

    #[test]
    fn store_layout_uses_shared_store_root() {
        let layout = StoreLayout::resolve(Path::new("/tmp/base"));
        assert_eq!(
            layout.image,
            PathBuf::from("/tmp/base/stores/nix-linux-aarch64/store.sparsebundle")
        );
        assert_eq!(
            layout.mount,
            PathBuf::from("/tmp/base/stores/nix-linux-aarch64/mount")
        );
        assert_eq!(
            layout.store,
            PathBuf::from("/tmp/base/stores/nix-linux-aarch64/mount/store")
        );
        assert_eq!(
            layout.var,
            PathBuf::from("/tmp/base/stores/nix-linux-aarch64/mount/var/nix")
        );
        assert_eq!(
            layout.profile_link(),
            PathBuf::from("/tmp/base/stores/nix-linux-aarch64/mount/profiles/default")
        );
    }

    #[test]
    fn is_ready_accepts_guest_absolute_profile_link() {
        let temp = tempfile::tempdir().unwrap();
        let layout = StoreLayout::resolve(temp.path());
        fs::create_dir_all(&layout.store).unwrap();
        fs::create_dir_all(&layout.profiles).unwrap();
        std::os::unix::fs::symlink("/nix/store/example-profile", layout.profile_link()).unwrap();

        assert!(layout.is_ready());
    }

    #[test]
    fn install_script_quotes_packages() {
        let script = install_script(
            &["nixpkgs#nodejs".to_string(), "weird'pkg".to_string()],
            &["node".to_string()],
        );
        assert!(script.contains("'nixpkgs#nodejs'"));
        assert!(script.contains("'weird'\\''pkg'"));
        assert!(script.contains("--profile \"$profile\""));
        assert!(script.contains("readlink -f \"$profile\""));
        assert!(script.contains("'/run/lnx/nix/profiles/default/bin/node' --version"));
        assert!(!script.contains("apt-get"));
        assert!(!script.contains("nixos.org/nix/install"));
        assert!(!script.contains("cp -a"));
    }
}
