use std::path::PathBuf;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Layout {
    pub base: PathBuf,
    pub instance: String,
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub instance_dir: PathBuf,
    pub snapshot_dir: PathBuf,
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
        let base = home.join(".lnx");
        let image_dir = base.join("images").join(instance);
        let instance_dir = base.join("instances").join(instance);
        let snapshot_dir = image_dir.join("memory-snapshots");
        let run_dir = instance_dir.clone();
        let kernel = kernel.unwrap_or_else(|| base.join("vmlinuz"));
        let rootfs = rootfs.unwrap_or_else(|| image_dir.join("rootfs.ext4"));
        let console_log = run_dir.join("console.log");

        Ok(Self {
            base,
            instance: instance.to_string(),
            kernel,
            rootfs,
            instance_dir,
            snapshot_dir,
            run_dir,
            console_log,
        })
    }
}
