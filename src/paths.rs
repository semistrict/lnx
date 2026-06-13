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
        let base = match std::env::var_os("LNX_BASE") {
            Some(path) => PathBuf::from(path),
            None => {
                let home = dirs::home_dir().context("could not resolve home directory")?;
                home.join(".lnx")
            }
        };
        let instance_dir = base.join("instances").join(instance);
        let snapshot_dir = instance_dir.join("memory-snapshots");
        let checkpoint_dir = instance_dir.join("checkpoints");
        let vm_initialized = instance_dir.join("vm-initialized");
        let run_dir = instance_dir.clone();
        let kernel = kernel.unwrap_or_else(|| base.join("vmlinuz"));
        let rootfs = rootfs.unwrap_or_else(|| instance_dir.join("rootfs.ext4"));
        let console_log = run_dir.join("console.log");

        Ok(Self {
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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_builds_per_instance_paths() {
        let layout = Layout::resolve("dev", None, None).expect("resolve layout");

        let home = dirs::home_dir().expect("home dir");
        assert_eq!(layout.base, home.join(".lnx"));
        assert_eq!(layout.instance, "dev");
        assert_eq!(layout.kernel, home.join(".lnx").join("vmlinuz"));
        assert_eq!(
            layout.rootfs,
            home.join(".lnx")
                .join("instances")
                .join("dev")
                .join("rootfs.ext4")
        );
        assert_eq!(
            layout.snapshot_dir,
            home.join(".lnx")
                .join("instances")
                .join("dev")
                .join("memory-snapshots")
        );
        assert_eq!(
            layout.checkpoint_dir,
            home.join(".lnx")
                .join("instances")
                .join("dev")
                .join("checkpoints")
        );
        assert_eq!(
            layout.vm_initialized,
            home.join(".lnx")
                .join("instances")
                .join("dev")
                .join("vm-initialized")
        );
        assert_eq!(
            layout.run_dir,
            home.join(".lnx").join("instances").join("dev")
        );
        assert_eq!(layout.console_log, layout.run_dir.join("console.log"));
    }

    #[test]
    fn resolve_honors_explicit_kernel_and_rootfs() {
        let kernel = PathBuf::from("/tmp/lnx-test-kernel");
        let rootfs = PathBuf::from("/tmp/lnx-test-rootfs.ext4");

        let layout =
            Layout::resolve("custom", Some(kernel.clone()), Some(rootfs.clone())).expect("layout");

        assert_eq!(layout.kernel, kernel);
        assert_eq!(layout.rootfs, rootfs);
        assert_eq!(layout.instance, "custom");
    }

    #[test]
    fn resolve_honors_lnx_base_env() {
        let base = std::env::temp_dir().join(format!("lnx-base-test-{}", std::process::id()));
        unsafe {
            std::env::set_var("LNX_BASE", &base);
        }
        let layout = Layout::resolve("envbase", None, None).expect("layout");
        unsafe {
            std::env::remove_var("LNX_BASE");
        }

        assert_eq!(layout.base, base);
        assert_eq!(layout.kernel, layout.base.join("vmlinuz"));
        assert_eq!(
            layout.rootfs,
            layout
                .base
                .join("instances")
                .join("envbase")
                .join("rootfs.ext4")
        );
    }
}
