use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

pub(crate) const INSTANCE_TRANSACTION_DIR: &str = "@lnx-transactions";

/// Instance names are made of ASCII letters, digits, `-`, `_` and `.`, so a
/// name is always one plain path component and a plain shell word.
pub(crate) fn validate_instance_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    {
        bail!(
            "invalid instance name {name:?}: use letters, digits, '-', '_' and '.'"
        );
    }
    Ok(())
}

/// Whether `name` could name an existing instance directory, including one
/// an older lnx created under a name that is no longer valid.
pub(crate) fn is_instance_dir_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/')
}
const INSTANCE_TRANSACTION_MARKER: &str = ".lnx-transactions-v1";
const INSTANCE_TRANSACTION_MARKER_CONTENT: &[u8] = b"lnx-instance-transactions-v1\n";

pub(crate) fn existing_instance_transaction_root(instances_root: &Path) -> Result<Option<PathBuf>> {
    Ok(instance_transaction_roots(instances_root)?
        .into_iter()
        .next())
}

pub(crate) fn instance_transaction_roots(instances_root: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(instances_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", instances_root.display()));
        }
    };
    let mut roots = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("read {}", instances_root.display()))?;
        if entry.file_type()?.is_dir() && is_instance_transaction_root(&entry.path()) {
            roots.push(entry.path());
        }
    }
    roots.sort();
    Ok(roots)
}

pub(crate) fn ensure_instance_transaction_root(instances_root: &Path) -> Result<PathBuf> {
    if let Some(root) = existing_instance_transaction_root(instances_root)? {
        return Ok(root);
    }
    fs::create_dir_all(instances_root)
        .with_context(|| format!("create {}", instances_root.display()))?;
    let mut attempt = 0_u64;
    let root = loop {
        let name = if attempt == 0 {
            INSTANCE_TRANSACTION_DIR.to_string()
        } else {
            format!("{INSTANCE_TRANSACTION_DIR}-{attempt}")
        };
        let candidate = instances_root.join(name);
        match fs::create_dir(&candidate) {
            Ok(()) => break candidate,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if is_instance_transaction_root(&candidate) {
                    return Ok(candidate);
                }
                attempt = attempt.saturating_add(1);
            }
            Err(error) => {
                return Err(error).with_context(|| format!("create {}", candidate.display()));
            }
        }
    };
    let marker = root.join(INSTANCE_TRANSACTION_MARKER);
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
            .with_context(|| format!("create {}", marker.display()))?;
        file.write_all(INSTANCE_TRANSACTION_MARKER_CONTENT)
            .with_context(|| format!("write {}", marker.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", marker.display()))?;
        fs::File::open(&root)
            .with_context(|| format!("open {}", root.display()))?
            .sync_all()
            .with_context(|| format!("sync {}", root.display()))?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_dir(&root);
        return Err(error);
    }
    Ok(root)
}

pub(crate) fn is_instance_transaction_root(path: &Path) -> bool {
    let valid_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name == INSTANCE_TRANSACTION_DIR
                || name
                    .strip_prefix(INSTANCE_TRANSACTION_DIR)
                    .and_then(|suffix| suffix.strip_prefix('-'))
                    .is_some_and(|suffix| {
                        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
                    })
        });
    valid_name
        && fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        && fs::read(path.join(INSTANCE_TRANSACTION_MARKER))
            .is_ok_and(|contents| contents == INSTANCE_TRANSACTION_MARKER_CONTENT)
}

/// Unix-domain sockets that a VM owner listens on or creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeSocket {
    Broker,
    CheckpointBroker,
    Agent,
    Snapshot,
    Control,
    Gvproxy,
}

impl RuntimeSocket {
    pub(crate) const ALL: [Self; 6] = [
        Self::Broker,
        Self::CheckpointBroker,
        Self::Agent,
        Self::Snapshot,
        Self::Control,
        Self::Gvproxy,
    ];

    pub(crate) fn file_name(self) -> &'static str {
        match self {
            Self::Broker => "broker.sock",
            Self::CheckpointBroker => "checkpoint-broker.sock",
            Self::Agent => "lnx-agent.sock",
            Self::Snapshot => "lnx-snapshot.sock",
            Self::Control => "lnx-control.sock",
            Self::Gvproxy => "gvproxy.sock",
        }
    }
}

/// Embedded gvproxy derives the socket libkrun connects to by appending this
/// suffix to the gvproxy socket path, which makes it the longest socket name.
pub(crate) const GVPROXY_KRUN_SOCKET_SUFFIX: &str = "-krun.sock";

/// Capacity of `sockaddr_un.sun_path`, including the terminating NUL
/// (104 bytes on macOS, 108 on Linux).
pub(crate) const UNIX_SOCKET_PATH_CAPACITY: usize = {
    // SAFETY: sockaddr_un is plain old data; only the array length is read.
    let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_path.len()
};

pub(crate) fn unix_socket_path_fits(path: &Path) -> bool {
    path.as_os_str().len() < UNIX_SOCKET_PATH_CAPACITY
}

fn longest_runtime_socket_path(dir: &Path) -> PathBuf {
    let gvproxy_krun = format!(
        "{}{GVPROXY_KRUN_SOCKET_SUFFIX}",
        RuntimeSocket::Gvproxy.file_name()
    );
    let longest = RuntimeSocket::ALL
        .into_iter()
        .map(|socket| socket.file_name().to_string())
        .chain([gvproxy_krun])
        .max_by_key(String::len)
        .expect("runtime sockets are not empty");
    dir.join(longest)
}

/// Chooses where an instance's sockets live. They stay next to the rest of
/// the runtime state when every socket path fits in `sun_path`; deep
/// project-local instance directories instead get a short per-user directory
/// keyed by the run directory, so the location is the same for every process
/// that resolves this instance.
fn socket_dir_for(run_dir: &Path, short_root: &Path) -> PathBuf {
    if unix_socket_path_fits(&longest_runtime_socket_path(run_dir)) {
        return run_dir.to_path_buf();
    }
    let digest = Sha256::digest(run_dir.as_os_str().as_encoded_bytes());
    let key: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    short_root.join(key)
}

/// A private per-user root for relocated sockets. macOS gives every user a
/// private `$TMPDIR`; elsewhere prefer `$XDG_RUNTIME_DIR` and fall back to a
/// uid-scoped directory under the shared temp dir.
fn short_socket_root() -> PathBuf {
    if cfg!(target_os = "macos") {
        return std::env::temp_dir().join("lnx");
    }
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("lnx"),
        _ => std::env::temp_dir().join(format!("lnx-{}", unsafe { libc::getuid() })),
    }
}

#[derive(Debug, Clone)]
pub struct Layout {
    pub base: PathBuf,
    pub instance: String,
    pub kernel: PathBuf,
    /// An image given with `--rootfs`: a new instance is created from a
    /// clone of it. An instance's own disk lives in its store.
    pub rootfs: Option<PathBuf>,
    pub instance_dir: PathBuf,
    pub run_dir: PathBuf,
    pub console_log: PathBuf,
}

impl Layout {
    pub fn resolve(
        instance: &str,
        kernel: Option<PathBuf>,
        rootfs: Option<PathBuf>,
    ) -> Result<Self> {
        let home = dirs::home_dir().context("could not resolve home directory")?;
        let cwd = std::env::current_dir().context("current directory")?;
        Ok(Self::resolve_with_env_and_cwd(
            instance,
            kernel,
            rootfs,
            std::env::var_os("LNX_BASE").map(PathBuf::from),
            std::env::var_os("LNX_RUN_BASE").map(PathBuf::from),
            home,
            cwd,
        ))
    }

    pub fn resolve_in_base(
        instance: &str,
        base: PathBuf,
        kernel: Option<PathBuf>,
        rootfs: Option<PathBuf>,
    ) -> Self {
        let kernel_base = std::env::var_os("LNX_BASE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(|| base.clone())
                    .join(".lnx")
            });
        Self::resolve_for_base(
            instance,
            kernel,
            rootfs,
            base,
            std::env::var_os("LNX_RUN_BASE").map(PathBuf::from),
            kernel_base,
        )
    }

    pub fn find_instance_base(instance: &str) -> Result<Option<PathBuf>> {
        let home = dirs::home_dir().context("could not resolve home directory")?;
        let cwd = std::env::current_dir().context("current directory")?;
        Ok(find_instance_base_between(instance, &cwd, &home))
    }

    #[cfg(test)]
    fn resolve_with_env(
        instance: &str,
        kernel: Option<PathBuf>,
        rootfs: Option<PathBuf>,
        base_env: Option<PathBuf>,
        run_base_env: Option<PathBuf>,
        home: PathBuf,
    ) -> Self {
        Self::resolve_with_env_and_cwd(
            instance,
            kernel,
            rootfs,
            base_env,
            run_base_env,
            home.clone(),
            home,
        )
    }

    pub(crate) fn socket_dir(&self) -> PathBuf {
        socket_dir_for(&self.run_dir, &short_socket_root())
    }

    pub(crate) fn socket(&self, socket: RuntimeSocket) -> PathBuf {
        self.socket_dir().join(socket.file_name())
    }

    /// Creates the run and socket directories. A relocated socket directory
    /// is private to the current user, and an existing one owned by someone
    /// else is refused rather than used.
    pub(crate) fn create_runtime_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.run_dir)
            .with_context(|| format!("create {}", self.run_dir.display()))?;
        let socket_dir = self.socket_dir();
        if socket_dir == self.run_dir {
            return Ok(());
        }
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&socket_dir)
            .with_context(|| format!("create {}", socket_dir.display()))?;
        let metadata = fs::symlink_metadata(&socket_dir)
            .with_context(|| format!("stat {}", socket_dir.display()))?;
        let uid = unsafe { libc::getuid() };
        if !metadata.is_dir() || metadata.uid() != uid {
            bail!(
                "socket directory {} is not a directory owned by uid {uid}",
                socket_dir.display()
            );
        }
        Ok(())
    }

    fn resolve_with_env_and_cwd(
        instance: &str,
        kernel: Option<PathBuf>,
        rootfs: Option<PathBuf>,
        base_env: Option<PathBuf>,
        run_base_env: Option<PathBuf>,
        home: PathBuf,
        cwd: PathBuf,
    ) -> Self {
        let base = base_env.clone().unwrap_or_else(|| {
            find_instance_base_between(instance, &cwd, &home).unwrap_or_else(|| home.join(".lnx"))
        });
        let kernel_base = if base_env.is_some() {
            base.clone()
        } else {
            home.join(".lnx")
        };
        Self::resolve_for_base(instance, kernel, rootfs, base, run_base_env, kernel_base)
    }

    fn resolve_for_base(
        instance: &str,
        kernel: Option<PathBuf>,
        rootfs: Option<PathBuf>,
        base: PathBuf,
        run_base_env: Option<PathBuf>,
        kernel_base: PathBuf,
    ) -> Self {
        let instance_dir = base.join("instances").join(instance);
        let run_dir = run_base_env
            .map(|base| base.join("instances").join(instance))
            .unwrap_or_else(|| instance_dir.clone());
        let kernel = kernel.unwrap_or_else(|| kernel_base.join("vmlinuz"));
        let console_log = run_dir.join("console.log");

        Self {
            base,
            instance: instance.to_string(),
            kernel,
            rootfs,
            instance_dir,
            run_dir,
            console_log,
        }
    }
}

fn find_instance_base_between(instance: &str, cwd: &Path, home: &Path) -> Option<PathBuf> {
    let mut cursor = Some(cwd);
    while let Some(dir) = cursor {
        let base = dir.join(".lnx");
        if instance_exists_in_base(&base, instance) {
            return Some(base);
        }
        if dir == home {
            return None;
        }
        cursor = dir.parent();
    }

    let base = home.join(".lnx");
    instance_exists_in_base(&base, instance).then_some(base)
}

fn instance_exists_in_base(base: &Path, instance: &str) -> bool {
    base.join("instances").join(instance).is_dir()
}

#[cfg(test)]
mod tests;
