use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

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
