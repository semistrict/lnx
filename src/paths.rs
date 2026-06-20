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

    pub fn current_dir_base() -> Result<PathBuf> {
        Ok(std::env::current_dir()
            .context("current directory")?
            .join(".lnx"))
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
mod tests {
    use super::*;
    use std::fs;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("lnx-paths-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn resolve_builds_per_instance_paths() {
        let home = PathBuf::from("/Users/test");
        let layout = Layout::resolve_with_env("dev", None, None, None, None, home.clone());

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

        let layout = Layout::resolve_with_env(
            "custom",
            Some(kernel.clone()),
            Some(rootfs.clone()),
            None,
            None,
            PathBuf::from("/Users/test"),
        );

        assert_eq!(layout.kernel, kernel);
        assert_eq!(layout.rootfs, rootfs);
        assert_eq!(layout.instance, "custom");
    }

    #[test]
    fn resolve_honors_lnx_base_env() {
        let base = std::env::temp_dir().join(format!("lnx-base-test-{}", std::process::id()));
        let layout = Layout::resolve_with_env(
            "envbase",
            None,
            None,
            Some(base.clone()),
            None,
            PathBuf::from("/Users/test"),
        );

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

    #[test]
    fn resolve_honors_lnx_run_base_env() {
        let base = std::env::temp_dir().join(format!("lnx-base-test-{}", std::process::id()));
        let run_base =
            std::env::temp_dir().join(format!("lnx-run-base-test-{}", std::process::id()));
        let layout = Layout::resolve_with_env(
            "dev",
            None,
            None,
            Some(base.clone()),
            Some(run_base.clone()),
            PathBuf::from("/Users/test"),
        );

        assert_eq!(layout.instance_dir, base.join("instances/dev"));
        assert_eq!(
            layout.snapshot_dir,
            base.join("instances/dev/memory-snapshots")
        );
        assert_eq!(layout.run_dir, run_base.join("instances/dev"));
        assert_eq!(layout.console_log, layout.run_dir.join("console.log"));
    }

    #[test]
    fn resolve_prefers_nearest_ancestor_with_requested_instance() {
        let temp = TempDir::new("ancestor");
        let home = temp.path.join("home");
        let project = home.join("work/project");
        let nested = project.join("src/module");
        fs::create_dir_all(project.join(".lnx").join("instances").join("dev"))
            .expect("create project instance");
        fs::create_dir_all(&nested).expect("create nested cwd");

        let layout =
            Layout::resolve_with_env_and_cwd("dev", None, None, None, None, home.clone(), nested);

        assert_eq!(layout.base, project.join(".lnx"));
        assert_eq!(layout.kernel, home.join(".lnx/vmlinuz"));
        assert_eq!(
            layout.rootfs,
            project.join(".lnx/instances/dev/rootfs.ext4")
        );
    }

    #[test]
    fn resolve_honors_lnx_base_for_kernel_store() {
        let base = PathBuf::from("/tmp/lnx-explicit-base");
        let layout = Layout::resolve_with_env(
            "dev",
            None,
            None,
            Some(base.clone()),
            None,
            PathBuf::from("/Users/test"),
        );

        assert_eq!(layout.kernel, base.join("vmlinuz"));
    }

    #[test]
    fn resolve_walks_past_ancestor_without_requested_instance() {
        let temp = TempDir::new("ancestor-miss");
        let home = temp.path.join("home");
        let project = home.join("work/project");
        let nested = project.join("src/module");
        fs::create_dir_all(project.join(".lnx").join("instances").join("other"))
            .expect("create project instance");
        fs::create_dir_all(home.join(".lnx").join("instances").join("dev"))
            .expect("create home instance");
        fs::create_dir_all(&nested).expect("create nested cwd");

        let layout =
            Layout::resolve_with_env_and_cwd("dev", None, None, None, None, home.clone(), nested);

        assert_eq!(layout.base, home.join(".lnx"));
    }

    #[test]
    fn resolve_in_base_places_new_instances_in_selected_store() {
        let base = PathBuf::from("/tmp/lnx-selected-base");
        let layout = Layout::resolve_in_base("new", base.clone(), None, None);

        assert_eq!(layout.base, base);
        assert_eq!(
            layout.rootfs,
            PathBuf::from("/tmp/lnx-selected-base/instances/new/rootfs.ext4")
        );
    }
}
