use std::{ffi::CString, path::Path, sync::OnceLock};

use anyhow::{Result, bail};

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
    pub(crate) id: u32,
}

impl Context {
    pub fn create() -> Result<Self> {
        let id = libkrun::krun_create_ctx();
        if id < 0 {
            bail_krun(id, "krun_create_ctx")?;
        }
        Ok(Self { id: id as u32 })
    }

    pub fn set_log_level(level: u32) -> Result<()> {
        static LOG_LEVEL: OnceLock<u32> = OnceLock::new();
        if LOG_LEVEL.set(level).is_err() {
            return Ok(());
        }
        call(libkrun::krun_set_log_level(level), "krun_set_log_level")
    }

    pub fn set_console_output(&self, path: &Path) -> Result<()> {
        let path = cstring_path(path)?;
        call(
            unsafe { libkrun::krun_set_console_output(self.id, path.as_ptr()) },
            "krun_set_console_output",
        )
    }

    pub fn set_vm_config(&self, cpus: u8, memory_mib: u32) -> Result<()> {
        call(
            libkrun::krun_set_vm_config(self.id, cpus, memory_mib),
            "krun_set_vm_config",
        )
    }

    pub fn set_nested_virt(&self, enabled: bool) -> Result<()> {
        call(
            unsafe { libkrun::krun_set_nested_virt(self.id, enabled) },
            "krun_set_nested_virt",
        )
    }

    pub fn set_kernel(&self, kernel: &Path, initrd: Option<&Path>, cmdline: &str) -> Result<()> {
        let kernel = cstring_path(kernel)?;
        let initrd = initrd.map(cstring_path).transpose()?;
        let cmdline = CString::new(cmdline)?;
        call(
            unsafe {
                libkrun::krun_set_kernel(
                    self.id,
                    kernel.as_ptr(),
                    KRUN_KERNEL_FORMAT_RAW,
                    initrd
                        .as_ref()
                        .map(|p| p.as_ptr())
                        .unwrap_or(std::ptr::null()),
                    cmdline.as_ptr(),
                )
            },
            "krun_set_kernel",
        )
    }

    pub fn add_root_pmem(&self, rootfs: &Path) -> Result<()> {
        let pmem_id = CString::new("rootfs")?;
        let rootfs = cstring_path(rootfs)?;
        call(
            unsafe { libkrun::krun_add_pmem(self.id, pmem_id.as_ptr(), rootfs.as_ptr(), false) },
            "krun_add_pmem(rootfs)",
        )
    }

    pub fn add_host_virtiofs(
        &self,
        tag: &str,
        path: &Path,
        write_allowlist: &[String],
        unshare_dir: &Path,
    ) -> Result<()> {
        let tag = CString::new(tag)?;
        let path = cstring_path(path)?;
        let unshare_dir = cstring_path(unshare_dir)?;
        call(
            unsafe {
                libkrun::krun_add_virtiofs4(
                    self.id,
                    tag.as_ptr(),
                    path.as_ptr(),
                    host_share_dax_window_bytes(),
                    false,
                    true,
                )
            },
            "krun_add_virtiofs4",
        )?;
        call(
            unsafe {
                libkrun::krun_set_virtiofs_unshare_dir(self.id, tag.as_ptr(), unshare_dir.as_ptr())
            },
            "krun_set_virtiofs_unshare_dir",
        )?;
        self.set_host_virtiofs_write_allowlist_cstr(&tag, write_allowlist)
    }

    #[allow(dead_code)]
    pub fn set_host_virtiofs_write_allowlist(&self, tag: &str, paths: &[String]) -> Result<()> {
        let tag = CString::new(tag)?;
        self.set_host_virtiofs_write_allowlist_cstr(&tag, paths)
    }

    fn set_host_virtiofs_write_allowlist_cstr(
        &self,
        tag: &CString,
        paths: &[String],
    ) -> Result<()> {
        let paths = CString::new(paths.join("\n"))?;
        call(
            unsafe {
                libkrun::krun_set_virtiofs_write_allowlist(self.id, tag.as_ptr(), paths.as_ptr())
            },
            "krun_set_virtiofs_write_allowlist",
        )
    }

    pub fn add_vsock_connector(&self, port: u32, socket: &Path) -> Result<()> {
        let socket = cstring_path(socket)?;
        call(
            unsafe { libkrun::krun_add_vsock_port2(self.id, port, socket.as_ptr(), false) },
            "krun_add_vsock_port2",
        )
    }

    pub fn add_gvproxy_network(&self, socket: &Path) -> Result<()> {
        let socket = cstring_path(socket)?;
        let mut mac = [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee];
        call(
            unsafe {
                libkrun::krun_add_net_unixgram(
                    self.id,
                    socket.as_ptr(),
                    -1,
                    mac.as_mut_ptr(),
                    COMPAT_NET_FEATURES,
                    NET_FLAG_VFKIT | NET_FLAG_DHCP_CLIENT,
                )
            },
            "krun_add_net_unixgram(gvproxy)",
        )
    }

    /// Attaches a connected datagram socket carrying one ethernet frame per
    /// datagram (the ingress daemon's vmnet pump). libkrun takes ownership
    /// of the fd.
    ///
    /// Unlike the gvproxy path, the vmnet bridge forwards raw ≤MTU ethernet
    /// frames and does no segmentation or checksum fixups, so no offload
    /// features are negotiated. The guest still adopts the configured
    /// per-instance MAC: libkrun always advertises VIRTIO_NET_F_MAC, which is
    /// not part of the configurable feature mask.
    #[cfg(target_os = "macos")]
    pub fn add_fd_network(&self, fd: std::os::fd::OwnedFd, mac: [u8; 6]) -> Result<()> {
        use std::os::fd::IntoRawFd;

        let mut mac = mac;
        call(
            unsafe {
                libkrun::krun_add_net_unixgram(
                    self.id,
                    std::ptr::null(),
                    fd.into_raw_fd(),
                    mac.as_mut_ptr(),
                    0,
                    0,
                )
            },
            "krun_add_net_unixgram(vmnet)",
        )
    }

    pub fn set_workdir(&self, path: &str) -> Result<()> {
        let path = CString::new(path)?;
        call(
            unsafe { libkrun::krun_set_workdir(self.id, path.as_ptr()) },
            "krun_set_workdir",
        )
    }

    pub fn set_exec(&self, path: &str, args: &[String], env: &[String]) -> Result<()> {
        let path = CString::new(path)?;
        let c_args = args
            .iter()
            .map(|arg| CString::new(arg.as_str()))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut argv = c_args.iter().map(|arg| arg.as_ptr()).collect::<Vec<_>>();
        argv.push(std::ptr::null());

        let c_env = env
            .iter()
            .map(|var| CString::new(var.as_str()))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut envp = c_env.iter().map(|var| var.as_ptr()).collect::<Vec<_>>();
        envp.push(std::ptr::null());

        call(
            unsafe { libkrun::krun_set_exec(self.id, path.as_ptr(), argv.as_ptr(), envp.as_ptr()) },
            "krun_set_exec",
        )
    }

    pub fn set_snapshot_path(&self, path: &Path) -> Result<()> {
        let path = cstring_path(path)?;
        call(
            unsafe { libkrun::krun_set_snapshot_path(self.id, path.as_ptr()) },
            "krun_set_snapshot_path",
        )
    }

    pub fn snapshot(&self, path: &Path) -> Result<()> {
        let path = cstring_path(path)?;
        call(
            unsafe { libkrun::krun_snapshot(self.id, path.as_ptr()) },
            "krun_snapshot",
        )
    }

    pub fn snapshot_with_file_copy(
        &self,
        path: &Path,
        copy_src: &Path,
        copy_dst_name: &str,
    ) -> Result<()> {
        let path = cstring_path(path)?;
        let copy_src = cstring_path(copy_src)?;
        let copy_dst_name = CString::new(copy_dst_name)?;
        call(
            unsafe {
                libkrun::krun_snapshot_with_file_copy(
                    self.id,
                    path.as_ptr(),
                    copy_src.as_ptr(),
                    copy_dst_name.as_ptr(),
                )
            },
            "krun_snapshot_with_file_copy",
        )
    }

    pub fn start_enter(&self) -> i32 {
        libkrun::krun_start_enter(self.id)
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

impl Drop for Context {
    fn drop(&mut self) {
        libkrun::krun_free_ctx(self.id);
    }
}

fn cstring_path(path: &Path) -> Result<CString> {
    CString::new(path.to_string_lossy().as_bytes()).map_err(Into::into)
}

fn call(rc: i32, name: &str) -> Result<()> {
    if rc < 0 {
        bail_krun(rc, name)?;
    }
    Ok(())
}

fn bail_krun<T>(rc: i32, name: &str) -> Result<T> {
    if rc < 0 {
        bail!("{name} failed: {}", std::io::Error::from_raw_os_error(-rc));
    }
    bail!("{name} failed with unexpected return code {rc}")
}
