use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

pub(crate) const INSTANCE_TRANSACTION_DIR: &str = "@lnx-transactions";
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

#[derive(Debug, Clone)]
pub struct Layout {
    pub base: PathBuf,
    pub instance: String,
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub instance_dir: PathBuf,
    pub snapshot_dir: PathBuf,
    pub checkpoint_dir: PathBuf,
    pub vm_initialized: PathBuf,
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
        let snapshot_dir = instance_dir.join("memory-snapshots");
        let checkpoint_dir = instance_dir.join("checkpoints");
        let vm_initialized = instance_dir.join("vm-initialized");
        let run_dir = run_base_env
            .map(|base| base.join("instances").join(instance))
            .unwrap_or_else(|| instance_dir.clone());
        let kernel = kernel.unwrap_or_else(|| kernel_base.join("vmlinuz"));
        let rootfs = rootfs.unwrap_or_else(|| instance_dir.join("rootfs.ext4"));
        let console_log = run_dir.join("console.log");

        Self {
            base,
            instance: instance.to_string(),
            kernel,
            rootfs,
            instance_dir,
            snapshot_dir,
            checkpoint_dir,
            vm_initialized,
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
