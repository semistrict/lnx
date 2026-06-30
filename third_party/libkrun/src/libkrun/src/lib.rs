#[macro_use]
extern crate log;

use crossbeam_channel::unbounded;
#[cfg(feature = "net")]
use devices::virtio::net::device::VirtioNetBackend;
use env_logger::Env;

#[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
use devices::virtio::fs::virtual_entry::{VirtualDirEntry, VirtualEntry, VirtualEntryContent};
use libc::{c_char, size_t};
use polly::event_manager::EventManager;
use std::collections::HashMap;
use std::convert::TryInto;
#[cfg(target_os = "linux")]
use std::env;
use std::ffi::CString;
#[cfg(all(target_arch = "x86_64", not(feature = "tee")))]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(feature = "aws-nitro")]
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use utils::eventfd::EventFd;
#[cfg(target_os = "macos")]
use utils::worker_message::WorkerMessage;
use vmm::resources::{TsiFlags, VmResources, VsockConfig};
#[cfg(not(feature = "tee"))]
use vmm::vmm_config::external_kernel::{ExternalKernel, KernelFormat};
#[cfg(not(feature = "tee"))]
use vmm::vmm_config::fs::FsDeviceConfig;
use vmm::vmm_config::kernel_bundle::KernelBundle;
#[cfg(feature = "tee")]
use vmm::vmm_config::kernel_bundle::{InitrdBundle, QbootBundle};
use vmm::vmm_config::kernel_cmdline::{DEFAULT_KERNEL_CMDLINE, KernelCmdlineConfig};
use vmm::vmm_config::machine_config::VmConfig;
#[cfg(feature = "net")]
use vmm::vmm_config::net::NetworkInterfaceConfig;
use vmm::vmm_config::pmem::PmemDeviceConfig;
use vmm::vmm_config::vsock::VsockDeviceConfig;

#[cfg(feature = "aws-nitro")]
use aws_nitro::enclave::NitroEnclave;

// krunfw library name for each context
#[cfg(all(target_os = "linux", not(feature = "tee")))]
const KRUNFW_NAME: &str = "libkrunfw.so.5";
#[cfg(all(target_os = "linux", feature = "amd-sev"))]
const KRUNFW_NAME: &str = "libkrunfw-sev.so.5";
#[cfg(all(target_os = "linux", feature = "tdx"))]
const KRUNFW_NAME: &str = "libkrunfw-tdx.so.5";
#[cfg(target_os = "macos")]
const KRUNFW_NAME: &str = "libkrunfw.5.dylib";

#[cfg(feature = "aws-nitro")]
static KRUN_NITRO_DEBUG: Mutex<bool> = Mutex::new(false);

// Path to the init binary to be executed inside the VM.
const INIT_PATH: &str = "/init.krun";

#[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
const DEFAULT_INIT_PAYLOAD: &[u8] = init_blob::INIT_BINARY;

#[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
fn init_virtual_entry() -> VirtualDirEntry {
    VirtualDirEntry {
        name: CString::new("init.krun").unwrap(),
        entry: VirtualEntry {
            mode: 0o755,
            one_shot: true,
            content: VirtualEntryContent::File {
                data: DEFAULT_INIT_PAYLOAD,
            },
        },
    }
}

/// Format used when configuring an external kernel.
#[cfg(not(feature = "tee"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelImageFormat {
    Raw,
    Elf,
    PeGz,
    ImageBz2,
    ImageGz,
    ImageZstd,
}

#[cfg(not(feature = "tee"))]
impl TryFrom<u32> for KernelImageFormat {
    type Error = ();

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Raw),
            1 => Ok(Self::Elf),
            2 => Ok(Self::PeGz),
            3 => Ok(Self::ImageBz2),
            4 => Ok(Self::ImageGz),
            5 => Ok(Self::ImageZstd),
            _ => Err(()),
        }
    }
}

#[cfg(not(feature = "tee"))]
impl From<KernelImageFormat> for u32 {
    fn from(value: KernelImageFormat) -> Self {
        match value {
            KernelImageFormat::Raw => 0,
            KernelImageFormat::Elf => 1,
            KernelImageFormat::PeGz => 2,
            KernelImageFormat::ImageBz2 => 3,
            KernelImageFormat::ImageGz => 4,
            KernelImageFormat::ImageZstd => 5,
        }
    }
}

type RunningVmm = Arc<Mutex<vmm::Vmm>>;
pub type KrunResult<T = ()> = std::result::Result<T, Error>;

/// A libkrun configuration and runtime context for Rust callers.
pub struct Context {
    cfg: Mutex<Option<ContextConfig>>,
    vmm: Mutex<Option<RunningVmm>>,
}

/// Error returned by the Rust API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error {
    errno: i32,
}

impl Error {
    fn from_errno(errno: i32) -> Self {
        Self { errno: errno.abs() }
    }

    pub fn raw_os_error(self) -> i32 {
        self.errno
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            std::io::Error::from_raw_os_error(self.raw_os_error())
        )
    }
}

impl std::error::Error for Error {}

impl Context {
    pub fn new() -> Self {
        Self {
            cfg: Mutex::new(Some(new_context_config())),
            vmm: Mutex::new(None),
        }
    }

    pub fn set_log_level(level: u32) -> KrunResult {
        set_log_level(level)
    }

    pub fn set_vm_config(&self, num_vcpus: u8, ram_mib: u32) -> KrunResult {
        self.with_cfg(|cfg| set_vm_config(cfg, num_vcpus, ram_mib))
    }

    pub fn set_console_output(&self, filepath: impl AsRef<Path>) -> KrunResult {
        self.with_cfg(|cfg| {
            if cfg.console_output.is_some() {
                Err(Error::from_errno(libc::EINVAL))
            } else {
                cfg.console_output = Some(filepath.as_ref().to_path_buf());
                Ok(())
            }
        })
    }

    pub fn set_nested_virt(&self, enabled: bool) -> KrunResult {
        self.with_cfg(|cfg| {
            cfg.vmr.nested_enabled = enabled;
            Ok(())
        })
    }

    #[cfg(not(feature = "tee"))]
    pub fn set_kernel(
        &self,
        path: impl AsRef<Path>,
        format: KernelImageFormat,
        initramfs_path: Option<impl AsRef<Path>>,
        cmdline: Option<&str>,
    ) -> KrunResult {
        let path = path.as_ref().to_path_buf();
        let format = match format {
            #[cfg(all(target_arch = "x86_64", not(feature = "tee")))]
            KernelImageFormat::Raw => {
                return self.with_cfg(|cfg| map_kernel_cfg(cfg, &path));
            }
            #[cfg(target_arch = "aarch64")]
            KernelImageFormat::Raw => KernelFormat::Raw,
            KernelImageFormat::Elf => KernelFormat::Elf,
            KernelImageFormat::PeGz => KernelFormat::PeGz,
            KernelImageFormat::ImageBz2 => KernelFormat::ImageBz2,
            KernelImageFormat::ImageGz => KernelFormat::ImageGz,
            KernelImageFormat::ImageZstd => KernelFormat::ImageZstd,
            #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
            KernelImageFormat::Raw => return Err(Error::from_errno(libc::EINVAL)),
        };

        let (initramfs_path, initramfs_size) = if let Some(initramfs_path) = initramfs_path {
            let initramfs_path = initramfs_path.as_ref().to_path_buf();
            let initramfs_size = std::fs::metadata(&initramfs_path)
                .map_err(|_| Error::from_errno(libc::EINVAL))?
                .len();
            (Some(initramfs_path), initramfs_size)
        } else {
            (None, 0)
        };

        let external_kernel = ExternalKernel {
            path,
            format,
            initramfs_path,
            initramfs_size,
            cmdline: cmdline.map(ToOwned::to_owned),
        };

        self.with_cfg(|cfg| {
            cfg.vmr.set_external_kernel(external_kernel);
            Ok(())
        })
    }

    pub fn add_pmem(
        &self,
        pmem_id: impl Into<String>,
        file_path: impl AsRef<Path>,
        read_only: bool,
    ) -> KrunResult {
        let pmem_cfg = PmemDeviceConfig {
            id: pmem_id.into(),
            path: file_path.as_ref().to_string_lossy().into_owned(),
            read_only,
        };
        self.with_cfg(|cfg| {
            cfg.add_pmem_cfg(pmem_cfg);
            Ok(())
        })
    }

    #[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
    pub fn add_virtiofs(
        &self,
        tag: impl Into<String>,
        path: Option<impl AsRef<Path>>,
        shm_size: u64,
        read_only: bool,
        write_allowlist: bool,
    ) -> KrunResult {
        let tag = tag.into();
        let shm_size = if shm_size > 0 {
            Some(
                shm_size
                    .try_into()
                    .map_err(|_| Error::from_errno(libc::EINVAL))?,
            )
        } else {
            None
        };
        let shared_dir = path.map(|path| path.as_ref().to_string_lossy().into_owned());
        self.with_cfg(|cfg| {
            let mut virtual_entries = Vec::new();
            if tag == "/dev/root" {
                virtual_entries.push(init_virtual_entry());
            }
            cfg.vmr.add_fs_device(FsDeviceConfig {
                fs_id: tag,
                shared_dir,
                shm_size,
                read_only,
                write_allowlist: write_allowlist.then(|| Arc::new(RwLock::new(Vec::new()))),
                unshare_dir: None,
                virtual_entries,
            });
            Ok(())
        })
    }

    #[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
    pub fn set_virtiofs_unshare_dir(&self, tag: &str, path: impl AsRef<Path>) -> KrunResult {
        let path = path.as_ref().to_path_buf();
        if let Some(vmm) = self.running_vmm() {
            return set_running_virtiofs_unshare_dir(&vmm, tag, path);
        }
        self.with_cfg(|cfg| set_cfg_virtiofs_unshare_dir(cfg, tag, path))
    }

    #[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
    pub fn set_virtiofs_write_allowlist(&self, tag: &str, paths: Vec<PathBuf>) -> KrunResult {
        if let Some(vmm) = self.running_vmm() {
            return set_running_virtiofs_write_allowlist(&vmm, tag, paths);
        }
        self.with_cfg(|cfg| set_cfg_virtiofs_write_allowlist(cfg, tag, paths))
    }

    #[cfg(feature = "vhost-user")]
    pub fn add_vhost_user_virtiofs(
        &self,
        tag: impl Into<String>,
        socket_path: impl AsRef<Path>,
    ) -> KrunResult {
        let tag = tag.into();
        let socket_path = socket_path.as_ref().to_string_lossy().into_owned();
        self.with_cfg(|cfg| add_vhost_user_virtiofs_config(cfg, &tag, socket_path))
    }

    #[cfg(not(feature = "vhost-user"))]
    pub fn add_vhost_user_virtiofs(
        &self,
        _tag: impl Into<String>,
        _socket_path: impl AsRef<Path>,
    ) -> KrunResult {
        Err(Error::from_errno(libc::ENOTSUP))
    }

    pub fn add_vsock_port(
        &self,
        port: u32,
        filepath: impl AsRef<Path>,
        listen: bool,
    ) -> KrunResult {
        #[cfg(feature = "aws-nitro")]
        if listen {
            return Err(Error::from_errno(libc::EINVAL));
        }

        let filepath = filepath.as_ref().to_path_buf();
        if listen {
            match filepath.try_exists() {
                Ok(true) => return Err(Error::from_errno(libc::EEXIST)),
                Err(_) => return Err(Error::from_errno(libc::EINVAL)),
                _ => {}
            }
        }

        self.with_cfg(|cfg| {
            if cfg.vsock_config == VsockConfig::Disabled {
                return Err(Error::from_errno(libc::ENODEV));
            }
            cfg.add_vsock_port(port, filepath, listen);
            Ok(())
        })
    }

    #[cfg(feature = "net")]
    pub fn add_net_unixgram(
        &self,
        path: impl AsRef<Path>,
        mac: [u8; 6],
        features: u32,
        flags: u32,
    ) -> KrunResult {
        if (features & !NET_ALL_FEATURES) != 0 || (flags & !NET_FLAG_ALL) != 0 {
            return Err(Error::from_errno(libc::EINVAL));
        }
        let send_vfkit_magic = flags & NET_FLAG_VFKIT != 0;
        let enable_dhcp_client = flags & NET_FLAG_DHCP_CLIENT != 0;
        let backend = VirtioNetBackend::UnixgramPath(path.as_ref().to_path_buf(), send_vfkit_magic);
        self.with_cfg(|cfg| {
            create_virtio_net(cfg, backend, mac, features);
            if enable_dhcp_client {
                cfg.vmr.dhcp_client = true;
            }
            Ok(())
        })
    }

    pub fn set_workdir(&self, workdir: impl Into<String>) -> KrunResult {
        let workdir = workdir.into();
        self.with_cfg(|cfg| {
            cfg.set_workdir(workdir);
            Ok(())
        })
    }

    pub fn set_exec(
        &self,
        exec_path: impl Into<String>,
        argv: &[String],
        envp: &[String],
    ) -> KrunResult {
        let exec_path = exec_path.into();
        let args = argv.join(" ");
        let env = envp.join(" ");
        self.with_cfg(|cfg| {
            cfg.set_exec_path(exec_path);
            cfg.set_env(env);
            cfg.set_args(args);
            Ok(())
        })
    }

    pub fn start_enter(&self) -> KrunResult {
        let cfg = self
            .cfg
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| Error::from_errno(libc::ENOENT))?;
        start_enter_context(cfg, |vmm| {
            *self.vmm.lock().unwrap() = Some(vmm);
        })
    }

    #[cfg(all(any(target_os = "macos", target_os = "linux"), target_arch = "aarch64"))]
    pub fn snapshot(&self, path: impl AsRef<Path>) -> KrunResult {
        snapshot_running_vmm(&self.require_running_vmm()?, path.as_ref())
    }

    #[cfg(not(all(any(target_os = "macos", target_os = "linux"), target_arch = "aarch64")))]
    pub fn snapshot(&self, _path: impl AsRef<Path>) -> KrunResult {
        Err(Error::from_errno(libc::ENOSYS))
    }

    #[cfg(all(any(target_os = "macos", target_os = "linux"), target_arch = "aarch64"))]
    pub fn snapshot_with_file_copy(
        &self,
        path: impl AsRef<Path>,
        copy_src: impl AsRef<Path>,
        copy_dst_name: impl AsRef<Path>,
    ) -> KrunResult {
        snapshot_running_vmm_with_file_copy(
            &self.require_running_vmm()?,
            path.as_ref(),
            copy_src.as_ref(),
            copy_dst_name.as_ref(),
        )
    }

    #[cfg(not(all(any(target_os = "macos", target_os = "linux"), target_arch = "aarch64")))]
    pub fn snapshot_with_file_copy(
        &self,
        _path: impl AsRef<Path>,
        _copy_src: impl AsRef<Path>,
        _copy_dst_name: impl AsRef<Path>,
    ) -> KrunResult {
        Err(Error::from_errno(libc::ENOSYS))
    }

    #[cfg(all(any(target_os = "macos", target_os = "linux"), target_arch = "aarch64"))]
    pub fn set_snapshot_path(&self, path: impl AsRef<Path>) -> KrunResult {
        let path = path.as_ref().to_path_buf();
        self.with_cfg(|cfg| {
            cfg.snapshot_restore_path = Some(path);
            Ok(())
        })
    }

    #[cfg(not(all(any(target_os = "macos", target_os = "linux"), target_arch = "aarch64")))]
    pub fn set_snapshot_path(&self, _path: impl AsRef<Path>) -> KrunResult {
        Err(Error::from_errno(libc::ENOSYS))
    }

    fn with_cfg(&self, f: impl FnOnce(&mut ContextConfig) -> KrunResult) -> KrunResult {
        let mut guard = self.cfg.lock().unwrap();
        let cfg = guard
            .as_mut()
            .ok_or_else(|| Error::from_errno(libc::ENOENT))?;
        f(cfg)
    }

    fn running_vmm(&self) -> Option<RunningVmm> {
        self.vmm.lock().unwrap().clone()
    }

    fn require_running_vmm(&self) -> KrunResult<RunningVmm> {
        self.running_vmm()
            .ok_or_else(|| Error::from_errno(libc::ENOENT))
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

static KRUNFW: LazyLock<Option<libloading::Library>> = LazyLock::new(|| unsafe {
    std::env::var_os("KRUNFW_PATH")
        .and_then(|path| libloading::Library::new(path).ok())
        .or_else(|| libloading::Library::new(KRUNFW_NAME).ok())
});

pub struct KrunfwBindings {
    get_kernel: libloading::Symbol<
        'static,
        unsafe extern "C" fn(*mut u64, *mut u64, *mut size_t) -> *mut c_char,
    >,
    #[cfg(feature = "tee")]
    get_initrd: libloading::Symbol<'static, unsafe extern "C" fn(*mut size_t) -> *mut c_char>,
    #[cfg(feature = "tee")]
    get_qboot: libloading::Symbol<'static, unsafe extern "C" fn(*mut size_t) -> *mut c_char>,
}

impl KrunfwBindings {
    fn load_bindings() -> Result<KrunfwBindings, libloading::Error> {
        let krunfw = match KRUNFW.as_ref() {
            Some(krunfw) => krunfw,
            None => return Err(libloading::Error::DlOpenUnknown),
        };
        Ok(unsafe {
            KrunfwBindings {
                get_kernel: krunfw.get(b"krunfw_get_kernel")?,
                #[cfg(feature = "tee")]
                get_initrd: krunfw.get(b"krunfw_get_initrd")?,
                #[cfg(feature = "tee")]
                get_qboot: krunfw.get(b"krunfw_get_qboot")?,
            }
        })
    }

    pub fn new() -> Option<Self> {
        Self::load_bindings().ok()
    }
}

#[derive(Default)]
struct ContextConfig {
    krunfw: Option<KrunfwBindings>,
    vmr: VmResources,
    workdir: Option<String>,
    exec_path: Option<String>,
    env: Option<String>,
    args: Option<String>,
    net_index: u8,
    vsock_config: VsockConfig,
    pmem_cfgs: Vec<PmemDeviceConfig>,
    #[cfg(feature = "tee")]
    tee_config_file: Option<PathBuf>,
    unix_ipc_port_map: Option<HashMap<u32, (PathBuf, bool)>>,
    shutdown_efd: Option<EventFd>,
    console_output: Option<PathBuf>,
    /// If set, `krun_start_enter` will restore from this snapshot directory
    /// instead of doing a fresh boot.
    #[cfg(all(any(target_os = "macos", target_os = "linux"), target_arch = "aarch64"))]
    snapshot_restore_path: Option<PathBuf>,
}

impl ContextConfig {
    fn set_workdir(&mut self, workdir: String) {
        self.workdir = Some(workdir);
    }

    fn get_workdir(&self) -> String {
        match &self.workdir {
            Some(workdir) => format!("KRUN_WORKDIR={workdir}"),
            None => "".to_string(),
        }
    }

    fn set_exec_path(&mut self, exec_path: String) {
        self.exec_path = Some(exec_path);
    }

    fn get_exec_path(&self) -> String {
        match &self.exec_path {
            Some(exec_path) => format!("KRUN_INIT={exec_path}"),
            None => "".to_string(),
        }
    }

    fn set_env(&mut self, env: String) {
        self.env = Some(env);
    }

    fn get_env(&self) -> String {
        match &self.env {
            Some(env) => env.clone(),
            None => "".to_string(),
        }
    }

    fn set_args(&mut self, args: String) {
        self.args = Some(args);
    }

    fn get_args(&self) -> String {
        match &self.args {
            Some(args) => args.clone(),
            None => "".to_string(),
        }
    }

    fn add_pmem_cfg(&mut self, pmem_cfg: PmemDeviceConfig) {
        self.pmem_cfgs.push(pmem_cfg);
    }

    #[cfg(feature = "tee")]
    fn get_tee_config_file(&self) -> Option<PathBuf> {
        self.tee_config_file.clone()
    }

    fn add_vsock_port(&mut self, port: u32, filepath: PathBuf, listen: bool) {
        if let Some(map) = &mut self.unix_ipc_port_map {
            map.insert(port, (filepath, listen));
        } else {
            let mut map: HashMap<u32, (PathBuf, bool)> = HashMap::new();
            map.insert(port, (filepath, listen));
            self.unix_ipc_port_map = Some(map);
        }
    }
}

#[cfg(feature = "aws-nitro")]
impl TryFrom<ContextConfig> for NitroEnclave {
    type Error = i32;

    fn try_from(ctx: ContextConfig) -> Result<Self, Self::Error> {
        let vm_config = ctx.vmr.vm_config();

        let Some(mem_size_mib) = vm_config.mem_size_mib else {
            error!("memory size not configured");
            return Err(-libc::EINVAL);
        };

        let Some(vcpus) = vm_config.vcpu_count else {
            error!("vCPU count not configured");
            return Err(-libc::EINVAL);
        };

        let rootfs = if let Some(path) = &ctx.vmr.fs.first() {
            path.shared_dir.clone()
        } else {
            error!("rootfs path required");
            return Err(-libc::EINVAL);
        };

        let Some(exec_path) = ctx.exec_path else {
            error!("exec path not specified");
            return Err(-libc::EINVAL);
        };

        let Some(exec_env) = ctx.env else {
            error!("execution env not specified");
            return Err(-libc::EINVAL);
        };

        let Some(exec_args) = ctx.args else {
            error!("execution args not specified");
            return Err(-libc::EINVAL);
        };

        let net_unixfd = {
            let mut list = ctx.vmr.net.list;
            let len = list.len();
            match len {
                0 => None,
                1 => {
                    let device = list.pop_front().unwrap();
                    let device = device.lock().unwrap();

                    let fd = match device.cfg_backend {
                        VirtioNetBackend::UnixstreamFd(fd) => RawFd::from(fd),
                        _ => return Err(libc::EINVAL),
                    };

                    Some(fd)
                }
                _ => {
                    error!(
                        "more than one network interface configured (max 1 allowed, found {len})"
                    );
                    return Err(-libc::EINVAL);
                }
            }
        };

        let Some(output_path) = ctx.console_output else {
            error!("console output path not specified");
            return Err(-libc::EINVAL);
        };

        let debug = KRUN_NITRO_DEBUG.lock().unwrap();

        Ok(Self {
            mem_size_mib,
            vcpus,
            rootfs,
            exec_path,
            exec_args,
            exec_env,
            net_unixfd,
            output_path,
            debug: *debug,
        })
    }
}

fn log_level_to_filter_str(level: u32) -> &'static str {
    match level {
        0 => "off",
        1 => "error",
        2 => "warn",
        3 => "info",
        4 => "debug",
        _ => "trace",
    }
}

fn set_log_level(level: u32) -> KrunResult {
    let filter = log_level_to_filter_str(level);
    let _ = env_logger::Builder::from_env(Env::default().default_filter_or(filter))
        .format_timestamp_micros()
        .try_init();

    #[cfg(feature = "aws-nitro")]
    {
        // Notify krun-awsnitro to enable debug for log level.
        if level == 4 {
            let mut debug = KRUN_NITRO_DEBUG.lock().unwrap();

            *debug = true;
        }
    }

    Ok(())
}

fn new_context_config() -> ContextConfig {
    let shutdown_efd = if cfg!(target_arch = "aarch64") && cfg!(target_os = "macos") {
        Some(EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap())
    } else {
        None
    };

    ContextConfig {
        krunfw: KrunfwBindings::new(),
        shutdown_efd,
        ..Default::default()
    }
}

fn set_vm_config(ctx_cfg: &mut ContextConfig, num_vcpus: u8, ram_mib: u32) -> KrunResult {
    let mem_size_mib: usize = match ram_mib.try_into() {
        Ok(size) => size,
        Err(e) => {
            warn!("Error parsing the amount of RAM: {e:?}");
            return Err(Error::from_errno(libc::EINVAL));
        }
    };

    let vm_config = VmConfig {
        vcpu_count: Some(num_vcpus),
        mem_size_mib: Some(mem_size_mib),
        ht_enabled: Some(false),
        cpu_template: None,
    };

    if ctx_cfg.vmr.set_vm_config(&vm_config).is_err() {
        return Err(Error::from_errno(libc::EINVAL));
    }

    Ok(())
}

#[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
fn set_running_virtiofs_write_allowlist(
    vmm: &RunningVmm,
    tag: &str,
    paths: Vec<PathBuf>,
) -> KrunResult {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return if vmm.lock().unwrap().set_virtiofs_write_allowlist(tag, paths) {
            Ok(())
        } else {
            Err(Error::from_errno(libc::ENOENT))
        };
    }

    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        let _ = (vmm, tag, paths);
        Err(Error::from_errno(libc::ENOENT))
    }
}

#[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
fn set_cfg_virtiofs_write_allowlist(
    cfg: &mut ContextConfig,
    tag: &str,
    paths: Vec<PathBuf>,
) -> KrunResult {
    let Some(fs) = cfg.vmr.fs.iter_mut().find(|fs| fs.fs_id == tag) else {
        return Err(Error::from_errno(libc::ENOENT));
    };
    let Some(allowlist) = &fs.write_allowlist else {
        return Err(Error::from_errno(libc::EINVAL));
    };
    *allowlist.write().unwrap() = paths;
    Ok(())
}

#[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
fn set_running_virtiofs_unshare_dir(vmm: &RunningVmm, tag: &str, path: PathBuf) -> KrunResult {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return if vmm.lock().unwrap().set_virtiofs_unshare_dir(tag, path) {
            Ok(())
        } else {
            Err(Error::from_errno(libc::ENOENT))
        };
    }

    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        let _ = (vmm, tag, path);
        Err(Error::from_errno(libc::ENOENT))
    }
}

#[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
fn set_cfg_virtiofs_unshare_dir(cfg: &mut ContextConfig, tag: &str, path: PathBuf) -> KrunResult {
    let Some(fs) = cfg.vmr.fs.iter_mut().find(|fs| fs.fs_id == tag) else {
        return Err(Error::from_errno(libc::ENOENT));
    };
    fs.unshare_dir = Some(path);
    Ok(())
}

/*
 * Send the VFKIT magic after establishing the connection,
 * as required by gvproxy in vfkit mode.
 */
#[cfg(feature = "net")]
const NET_FLAG_VFKIT: u32 = 1 << 0;
#[cfg(feature = "net")]
const NET_FLAG_DHCP_CLIENT: u32 = 1 << 1;
#[cfg(feature = "net")]
const NET_FLAG_ALL: u32 = NET_FLAG_VFKIT | NET_FLAG_DHCP_CLIENT;

/* Taken from uapi/linux/virtio_net.h */
#[cfg(feature = "net")]
const NET_FEATURE_CSUM: u32 = 1 << 0;
#[cfg(feature = "net")]
const NET_FEATURE_GUEST_CSUM: u32 = 1 << 1;
#[cfg(feature = "net")]
const NET_FEATURE_GUEST_TSO4: u32 = 1 << 7;
#[cfg(feature = "net")]
const NET_FEATURE_GUEST_TSO6: u32 = 1 << 8;
#[cfg(feature = "net")]
const NET_FEATURE_GUEST_UFO: u32 = 1 << 10;
#[cfg(feature = "net")]
const NET_FEATURE_HOST_TSO4: u32 = 1 << 11;
#[cfg(feature = "net")]
const NET_FEATURE_HOST_TSO6: u32 = 1 << 12;
#[cfg(feature = "net")]
const NET_FEATURE_HOST_UFO: u32 = 1 << 14;
#[cfg(feature = "net")]
const NET_ALL_FEATURES: u32 = NET_FEATURE_CSUM
    | NET_FEATURE_GUEST_CSUM
    | NET_FEATURE_GUEST_TSO4
    | NET_FEATURE_GUEST_TSO6
    | NET_FEATURE_GUEST_UFO
    | NET_FEATURE_HOST_TSO4
    | NET_FEATURE_HOST_TSO6
    | NET_FEATURE_HOST_UFO;

#[cfg(feature = "vhost-user")]
fn add_vhost_user_virtiofs_config(
    cfg: &mut ContextConfig,
    tag: &str,
    socket_path: String,
) -> KrunResult {
    use vmm::resources::VhostUserDeviceConfig;

    if tag.is_empty() || tag.as_bytes().len() > 36 || socket_path.is_empty() {
        return Err(Error::from_errno(libc::EINVAL));
    }

    let mut config_space = vec![0_u8; 40];
    config_space[..tag.as_bytes().len()].copy_from_slice(tag.as_bytes());
    config_space[36..40].copy_from_slice(&1_u32.to_le_bytes());

    cfg.vmr.vhost_user_devices.push(VhostUserDeviceConfig {
        device_type: 26,
        socket_path,
        name: Some(format!("vhost-user-fs-{tag}")),
        num_queues: 2,
        queue_sizes: vec![1024, 1024],
        config_space: Some(config_space),
    });
    Ok(())
}

#[cfg(feature = "net")]
fn create_virtio_net(
    ctx_cfg: &mut ContextConfig,
    backend: VirtioNetBackend,
    mac: [u8; 6],
    features: u32,
) {
    let network_interface_config = NetworkInterfaceConfig {
        iface_id: format!("eth{}", ctx_cfg.net_index),
        backend,
        mac,
        features,
    };
    ctx_cfg.net_index += 1;
    ctx_cfg
        .vmr
        .add_network_interface(network_interface_config)
        .expect("Failed to create network interface");
}

#[cfg(all(target_arch = "x86_64", not(feature = "tee")))]
fn map_kernel_cfg(ctx_cfg: &mut ContextConfig, kernel_path: &PathBuf) -> KrunResult {
    let file = match File::options().read(true).write(false).open(kernel_path) {
        Ok(file) => file,
        Err(err) => {
            error!("Error opening external kernel: {err}");
            return Err(Error::from_errno(libc::EINVAL));
        }
    };

    let kernel_size = file.metadata().unwrap().len();

    let kernel_host_addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            kernel_size as usize,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0_i64,
        )
    };
    if std::ptr::eq(kernel_host_addr, libc::MAP_FAILED) {
        error!("Can't load kernel into process map");
        return Err(Error::from_errno(libc::EINVAL));
    }

    let kernel_bundle = KernelBundle {
        host_addr: kernel_host_addr as u64,
        guest_addr: 0x8000_0000,
        entry_addr: 0x8000_0000,
        size: kernel_size as usize,
    };

    ctx_cfg.vmr.set_kernel_bundle(kernel_bundle).unwrap();

    Ok(())
}

unsafe fn load_krunfw_payload(
    krunfw: &KrunfwBindings,
    vmr: &mut VmResources,
) -> Result<(), libloading::Error> {
    let mut kernel_guest_addr: u64 = 0;
    let mut kernel_entry_addr: u64 = 0;
    let mut kernel_size: usize = 0;
    let kernel_host_addr = unsafe {
        (krunfw.get_kernel)(
            &mut kernel_guest_addr as *mut u64,
            &mut kernel_entry_addr as *mut u64,
            &mut kernel_size as *mut usize,
        )
    };
    let kernel_bundle = KernelBundle {
        host_addr: kernel_host_addr as u64,
        guest_addr: kernel_guest_addr,
        entry_addr: kernel_entry_addr,
        size: kernel_size,
    };
    vmr.set_kernel_bundle(kernel_bundle).unwrap();

    #[cfg(feature = "tee")]
    {
        let mut qboot_size: usize = 0;
        let qboot_host_addr = unsafe { (krunfw.get_qboot)(&mut qboot_size as *mut usize) };
        let qboot_bundle = QbootBundle {
            host_addr: qboot_host_addr as u64,
            size: qboot_size,
        };
        vmr.set_qboot_bundle(qboot_bundle).unwrap();

        let mut initrd_size: usize = 0;
        let initrd_host_addr = unsafe { (krunfw.get_initrd)(&mut initrd_size as *mut usize) };
        let initrd_bundle = InitrdBundle {
            host_addr: initrd_host_addr as u64,
            size: initrd_size,
        };
        vmr.set_initrd_bundle(initrd_bundle).unwrap();
    }

    Ok(())
}

#[allow(unreachable_code)]
fn start_enter_context(
    mut ctx_cfg: ContextConfig,
    store_running_vmm: impl FnOnce(RunningVmm),
) -> KrunResult {
    vmm::timing_event("start_enter.entry");
    #[cfg(target_os = "linux")]
    {
        let prname = match env::var("HOSTNAME") {
            Ok(val) => CString::new(format!("VM:{val}")).unwrap(),
            Err(_) => CString::new("libkrun VM").unwrap(),
        };
        unsafe { libc::prctl(libc::PR_SET_NAME, prname.as_ptr()) };
    }

    #[cfg(feature = "aws-nitro")]
    return krun_start_enter_nitro(ctx_cfg);

    let mut event_manager = match EventManager::new() {
        Ok(em) => em,
        Err(e) => {
            error!("Unable to create EventManager: {e:?}");
            return Err(Error::from_errno(libc::EINVAL));
        }
    };
    vmm::timing_event("start_enter.event_manager.created");

    vmm::timing_event("start_enter.ctx.loaded");

    if ctx_cfg.vmr.external_kernel.is_none()
        && ctx_cfg.vmr.kernel_bundle.is_none()
        && ctx_cfg.vmr.firmware_config.is_none()
    {
        if let Some(ref krunfw) = ctx_cfg.krunfw {
            if let Err(err) = unsafe { load_krunfw_payload(krunfw, &mut ctx_cfg.vmr) } {
                eprintln!("Can't load libkrunfw symbols: {err}");
                return Err(Error::from_errno(libc::ENOENT));
            }
        } else {
            eprintln!("Couldn't find or load {KRUNFW_NAME}");
            return Err(Error::from_errno(libc::ENOENT));
        }
    }

    for pmem_cfg in ctx_cfg.pmem_cfgs.clone() {
        ctx_cfg.vmr.add_pmem_device(pmem_cfg);
    }
    vmm::timing_event("start_enter.pmem.configured");

    /*
     * Before the VM is started in an encrypted context, the TEE config must
     * be set. If it is not set by this point, print the relevant error
     * message and fail.
     */
    #[cfg(feature = "tee")]
    if let Some(tee_config) = ctx_cfg.get_tee_config_file() {
        if let Err(e) = ctx_cfg.vmr.set_tee_config(tee_config) {
            error!("Error setting up TEE config: {e:?}");
            return Err(Error::from_errno(libc::EINVAL));
        }
    } else {
        error!("Missing TEE config file");
        return Err(Error::from_errno(libc::EINVAL));
    }

    let kernel_cmdline = KernelCmdlineConfig {
        prolog: Some(format!("{DEFAULT_KERNEL_CMDLINE} init={INIT_PATH}")),
        krun_env: Some(format!(
            " {} {} {}",
            ctx_cfg.get_exec_path(),
            ctx_cfg.get_workdir(),
            ctx_cfg.get_env(),
        )),
        epilog: Some(format!(" -- {}", ctx_cfg.get_args())),
    };

    if ctx_cfg.vmr.set_kernel_cmdline(kernel_cmdline).is_err() {
        return Err(Error::from_errno(libc::EINVAL));
    }
    vmm::timing_event("start_enter.kernel_cmdline.configured");

    match &ctx_cfg.vsock_config {
        VsockConfig::Disabled => (),
        VsockConfig::Explicit { tsi_flags } => {
            let vsock_device_config = VsockDeviceConfig {
                vsock_id: "vsock0".to_string(),
                guest_cid: 3,
                host_port_map: None,
                unix_ipc_port_map: ctx_cfg.unix_ipc_port_map.clone(),
                tsi_flags: *tsi_flags,
            };
            ctx_cfg.vmr.set_vsock_device(vsock_device_config).unwrap();
        }
        VsockConfig::Implicit => {
            // Implicit vsock configuration - use heuristics
            // Check if TSI should be enabled based on network configuration
            #[cfg(feature = "net")]
            let enable_tsi = ctx_cfg.vmr.net.list.is_empty();
            #[cfg(not(feature = "net"))]
            let enable_tsi = true;

            let has_ipc_map = ctx_cfg.unix_ipc_port_map.is_some();

            if enable_tsi || has_ipc_map {
                let (tsi_flags, host_port_map) = if enable_tsi {
                    (TsiFlags::HIJACK_INET, None)
                } else {
                    (TsiFlags::empty(), None)
                };

                let vsock_device_config = VsockDeviceConfig {
                    vsock_id: "vsock0".to_string(),
                    guest_cid: 3,
                    host_port_map,
                    unix_ipc_port_map: ctx_cfg.unix_ipc_port_map.clone(),
                    tsi_flags,
                };
                ctx_cfg.vmr.set_vsock_device(vsock_device_config).unwrap();
            }
        }
    }
    vmm::timing_event("start_enter.vsock.configured");

    if let Some(console_output) = ctx_cfg.console_output {
        ctx_cfg.vmr.set_console_output(console_output);
    }
    vmm::timing_event("start_enter.resources.finalized");

    let (sender, _receiver) = unbounded();
    #[cfg(target_os = "macos")]
    let worker_sender = sender.clone();

    #[cfg(all(any(target_os = "macos", target_os = "linux"), target_arch = "aarch64"))]
    {
        ctx_cfg.vmr.snapshot_restore_path = ctx_cfg.snapshot_restore_path.clone();
    }
    vmm::timing_event("start_enter.build_microvm.begin");

    let _vmm = match vmm::builder::build_microvm(
        &ctx_cfg.vmr,
        &mut event_manager,
        ctx_cfg.shutdown_efd,
        sender,
    ) {
        Ok(vmm) => vmm,
        Err(e) => {
            error!("Building the microVM failed: {e:?}");
            return Err(Error::from_errno(libc::EINVAL));
        }
    };
    vmm::timing_event("start_enter.build_microvm.done");

    store_running_vmm(_vmm.clone());
    vmm::timing_event("start_enter.running_vmm.registered");

    #[cfg(target_os = "macos")]
    let macos_worker_needed = ctx_cfg.vmr.fs.iter().any(|fs| fs.shm_size.is_some());

    #[cfg(target_os = "macos")]
    if macos_worker_needed {
        vmm::worker::start_worker_thread(_vmm.clone(), _receiver.clone()).unwrap();
        vmm::timing_event("start_enter.vmm_worker.started");
    }

    #[cfg(all(any(target_os = "macos", target_os = "linux"), target_arch = "aarch64"))]
    if ctx_cfg.snapshot_restore_path.is_some() {
        #[cfg(target_os = "macos")]
        if macos_worker_needed {
            let (reply_sender, reply_receiver) = unbounded();
            if let Err(e) = worker_sender.send(WorkerMessage::Barrier(reply_sender)) {
                error!("Error sending restore worker barrier: {e:?}");
                return Err(Error::from_errno(libc::EINVAL));
            }
            match reply_receiver.recv() {
                Ok(true) => vmm::timing_event("start_enter.restore.worker_barrier"),
                Ok(false) => {
                    error!("restore worker barrier failed");
                    return Err(Error::from_errno(libc::EINVAL));
                }
                Err(e) => {
                    error!("Error waiting for restore worker barrier: {e:?}");
                    return Err(Error::from_errno(libc::EINVAL));
                }
            }
        }
        match event_manager.run_with_timeout(0) {
            Ok(count) => {
                vmm::timing_event(&format!(
                    "start_enter.restore.event_manager_primed count={count}"
                ));
            }
            Err(e) => {
                error!("Error priming EventManager before snapshot restore resume: {e:?}");
                return Err(Error::from_errno(libc::EINVAL));
            }
        }
        _vmm.lock().unwrap().replay_restore_notifications();
        vmm::timing_event("start_enter.restore.notifications_replayed");
        if let Err(e) = _vmm.lock().unwrap().resume_after_restore() {
            error!("snapshot restore resume failed: {e}");
            return Err(Error::from_errno(libc::EINVAL));
        }
        vmm::timing_event("start_enter.restore.resumed");
    }

    #[cfg(target_arch = "x86_64")]
    if ctx_cfg.vmr.split_irqchip {
        vmm::worker::start_worker_thread(_vmm.clone(), _receiver.clone()).unwrap();
    }

    #[cfg(any(feature = "amd-sev", feature = "tdx"))]
    vmm::worker::start_worker_thread(_vmm.clone(), _receiver.clone()).unwrap();

    vmm::timing_event("start_enter.event_loop.begin");
    loop {
        match event_manager.run() {
            Ok(_) => {}
            Err(e) => {
                error!("Error in EventManager loop: {e:?}");
                return Err(Error::from_errno(libc::EINVAL));
            }
        }
    }
}

#[cfg(feature = "aws-nitro")]
fn krun_start_enter_nitro(ctx_cfg: ContextConfig) -> KrunResult {
    let Ok(enclave) = NitroEnclave::try_from(ctx_cfg) else {
        return Err(Error::from_errno(libc::EINVAL));
    };

    match enclave.run() {
        Ok(0) => Ok(()),
        Ok(code) if code < 0 => Err(Error::from_errno(-code)),
        Ok(_) => Err(Error::from_errno(libc::EINVAL)),
        Err(e) => {
            error!("Error running nitro enclave: {e}");

            Err(Error::from_errno(libc::EINVAL))
        }
    }
}

#[cfg(all(any(target_os = "macos", target_os = "linux"), target_arch = "aarch64"))]
fn snapshot_running_vmm(vmm: &RunningVmm, path: &Path) -> KrunResult {
    let result = vmm.lock().unwrap().snapshot(path);
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            error!("krun_snapshot failed: {e}");
            if e.contains("device refused") || e.contains("connections") {
                Err(Error::from_errno(libc::EPERM))
            } else {
                Err(Error::from_errno(libc::EIO))
            }
        }
    }
}

#[cfg(all(any(target_os = "macos", target_os = "linux"), target_arch = "aarch64"))]
fn snapshot_running_vmm_with_file_copy(
    vmm: &RunningVmm,
    path: &Path,
    copy_src: &Path,
    copy_dst_name: &Path,
) -> KrunResult {
    if copy_dst_name.is_absolute() || copy_dst_name.components().count() != 1 {
        return Err(Error::from_errno(libc::EINVAL));
    }
    let result = vmm
        .lock()
        .unwrap()
        .snapshot_with_file_copy(path, copy_src, copy_dst_name);
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            error!("krun_snapshot_with_file_copy failed: {e}");
            if e.contains("device refused") || e.contains("connections") {
                Err(Error::from_errno(libc::EPERM))
            } else {
                Err(Error::from_errno(libc::EIO))
            }
        }
    }
}
