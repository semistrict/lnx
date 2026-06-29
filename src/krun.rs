use std::{path::Path, sync::OnceLock};

use anyhow::Result;

pub const KRUN_KERNEL_FORMAT_RAW: u32 = 0;
const COMPAT_NET_FEATURES: u32 = (1 << 0) | (1 << 1) | (1 << 7) | (1 << 10) | (1 << 11) | (1 << 14);
const NET_FLAG_VFKIT: u32 = 1 << 0;
const NET_FLAG_DHCP_CLIENT: u32 = 1 << 1;
const VIRTIOFS_DAX_WINDOW_BYTES: u64 = 8 << 30;
/// Opt back into the old host-share DAX path. Writable host-share DAX can
/// wedge SQLite WAL close/unmap paths on macOS, so the default is the safer
/// non-DAX virtio-fs mount.
pub(crate) const HOST_SHARE_DAX_ENV: &str = "LNX_HOST_SHARE_DAX";

pub struct Context {
    inner: libkrun::Context,
}

impl Context {
    pub fn create() -> Result<Self> {
        let inner = libkrun::Context::create().map_err(|rc| krun_error(rc, "Context::create"))?;
        Ok(Self { inner })
    }

    pub fn set_log_level(level: u32) -> Result<()> {
        static LOG_LEVEL: OnceLock<u32> = OnceLock::new();
        if LOG_LEVEL.set(level).is_err() {
            return Ok(());
        }
        call(
            libkrun::Context::set_log_level(level),
            "Context::set_log_level",
        )
    }

    pub fn set_console_output(&self, path: &Path) -> Result<()> {
        call(
            self.inner.set_console_output(path),
            "Context::set_console_output",
        )
    }

    pub fn set_vm_config(&self, cpus: u8, memory_mib: u32) -> Result<()> {
        call(
            self.inner.set_vm_config(cpus, memory_mib),
            "Context::set_vm_config",
        )
    }

    pub fn set_nested_virt(&self, enabled: bool) -> Result<()> {
        call(
            self.inner.set_nested_virt(enabled),
            "Context::set_nested_virt",
        )
    }

    pub fn set_kernel(&self, kernel: &Path, initrd: Option<&Path>, cmdline: &str) -> Result<()> {
        call(
            self.inner.set_kernel(
                kernel,
                libkrun::KernelImageFormat::try_from(KRUN_KERNEL_FORMAT_RAW)
                    .expect("raw kernel format is supported"),
                initrd,
                Some(cmdline),
            ),
            "Context::set_kernel",
        )
    }

    pub fn add_root_pmem(&self, rootfs: &Path) -> Result<()> {
        call(
            self.inner.add_pmem("rootfs", rootfs, false),
            "Context::add_pmem(rootfs)",
        )
    }

    pub fn add_virtiofs(&self, tag: &str, path: &Path, read_only: bool) -> Result<()> {
        call(
            self.inner.add_virtiofs(
                tag,
                Some(path),
                host_share_dax_window_bytes(),
                read_only,
                false,
            ),
            "Context::add_virtiofs",
        )
    }

    pub fn add_host_virtiofs(
        &self,
        tag: &str,
        path: &Path,
        write_allowlist: &[String],
        unshare_dir: &Path,
    ) -> Result<()> {
        call(
            self.inner
                .add_virtiofs(tag, Some(path), host_share_dax_window_bytes(), false, true),
            "Context::add_virtiofs",
        )?;
        call(
            self.inner.set_virtiofs_unshare_dir(tag, unshare_dir),
            "Context::set_virtiofs_unshare_dir",
        )?;
        self.set_host_virtiofs_write_allowlist(tag, write_allowlist)
    }

    #[allow(dead_code)]
    pub fn set_host_virtiofs_write_allowlist(&self, tag: &str, paths: &[String]) -> Result<()> {
        call(
            self.inner.set_virtiofs_write_allowlist(
                tag,
                paths.iter().map(Path::new).map(Into::into).collect(),
            ),
            "Context::set_virtiofs_write_allowlist",
        )
    }

    pub fn add_vsock_connector(&self, port: u32, socket: &Path) -> Result<()> {
        call(
            self.inner.add_vsock_port(port, socket, false),
            "Context::add_vsock_port",
        )
    }

    pub fn add_gvproxy_network(&self, socket: &Path) -> Result<()> {
        let mac = [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee];
        call(
            self.inner.add_net_unixgram(
                socket,
                mac,
                COMPAT_NET_FEATURES,
                NET_FLAG_VFKIT | NET_FLAG_DHCP_CLIENT,
            ),
            "Context::add_net_unixgram(gvproxy)",
        )
    }

    pub fn set_workdir(&self, path: &str) -> Result<()> {
        call(self.inner.set_workdir(path), "Context::set_workdir")
    }

    pub fn set_exec(&self, path: &str, args: &[String], env: &[String]) -> Result<()> {
        call(self.inner.set_exec(path, args, env), "Context::set_exec")
    }

    pub fn set_snapshot_path(&self, path: &Path) -> Result<()> {
        call(
            self.inner.set_snapshot_path(path),
            "Context::set_snapshot_path",
        )
    }

    pub fn snapshot(&self, path: &Path) -> Result<()> {
        call(self.inner.snapshot(path), "Context::snapshot")
    }

    pub fn snapshot_with_file_copy(
        &self,
        path: &Path,
        copy_src: &Path,
        copy_dst_name: &str,
    ) -> Result<()> {
        call(
            self.inner
                .snapshot_with_file_copy(path, copy_src, Path::new(copy_dst_name)),
            "Context::snapshot_with_file_copy",
        )
    }

    pub fn start_enter(&self) -> i32 {
        self.inner.start_enter()
    }
}

pub(crate) fn host_share_dax_enabled() -> bool {
    matches!(
        std::env::var(HOST_SHARE_DAX_ENV).as_deref(),
        Ok("1" | "true" | "on" | "yes")
    )
}

fn host_share_dax_window_bytes() -> u64 {
    if host_share_dax_enabled() {
        VIRTIOFS_DAX_WINDOW_BYTES
    } else {
        0
    }
}

fn call(result: std::result::Result<(), i32>, name: &str) -> Result<()> {
    result.map_err(|rc| krun_error(rc, name))
}

fn krun_error(rc: i32, name: &str) -> anyhow::Error {
    if rc < 0 {
        anyhow::anyhow!("{name} failed: {}", std::io::Error::from_raw_os_error(-rc))
    } else {
        anyhow::anyhow!("{name} failed with unexpected return code {rc}")
    }
}
