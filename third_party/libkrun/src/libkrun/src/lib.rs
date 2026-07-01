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
#[cfg(all(target_arch = "x86_64", not(feature = "tee")))]
use std::os::fd::AsRawFd;
#[cfg(feature = "aws-nitro")]
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use utils::eventfd::EventFd;
#[cfg(target_os = "macos")]
use utils::worker_message::WorkerMessage;
use vmm::resources::{TsiFlags, VirtioConsoleConfigMode, VmResources, VsockConfig};
#[cfg(all(not(feature = "tee"), target_arch = "aarch64"))]
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

#[cfg(all(
    feature = "init-blob",
    not(any(feature = "tee", feature = "aws-nitro"))
))]
const DEFAULT_INIT_PAYLOAD: &[u8] = init_blob::INIT_BINARY;

#[cfg(all(
    feature = "init-blob",
    not(any(feature = "tee", feature = "aws-nitro"))
))]
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

type RunningVmm = Arc<Mutex<vmm::Vmm>>;
pub type KrunResult<T = ()> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn as_filter_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

/// Error returned by the Rust API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidConfig,
    NotFound,
    AlreadyExists,
    NotStarted,
    Unsupported,
    PermissionDenied,
    Io,
    Other(std::io::ErrorKind),
}

impl Error {
    fn from_errno(errno: i32) -> Self {
        match errno.abs() {
            libc::EINVAL => Self::InvalidConfig,
            libc::ENOENT => Self::NotFound,
            libc::EEXIST => Self::AlreadyExists,
            libc::ENODEV => Self::NotStarted,
            libc::ENOTSUP | libc::ENOSYS => Self::Unsupported,
            libc::EPERM => Self::PermissionDenied,
            libc::EIO => Self::Io,
            errno => Self::Other(std::io::Error::from_raw_os_error(errno).kind()),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfig => write!(f, "invalid VM configuration"),
            Self::NotFound => write!(f, "requested VM resource was not found"),
            Self::AlreadyExists => write!(f, "VM resource already exists"),
            Self::NotStarted => write!(f, "VM is not running yet"),
            Self::Unsupported => write!(f, "operation is not supported on this platform"),
            Self::PermissionDenied => write!(f, "VM device refused the operation"),
            Self::Io => write!(f, "VM I/O operation failed"),
            Self::Other(kind) => write!(f, "VM operation failed ({kind:?})"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(not(feature = "tee"))]
pub struct Kernel {
    path: PathBuf,
    initramfs_path: Option<PathBuf>,
    cmdline: Option<String>,
}

#[cfg(not(feature = "tee"))]
impl Kernel {
    pub fn raw(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            initramfs_path: None,
            cmdline: None,
        }
    }

    pub fn initramfs(mut self, path: impl AsRef<Path>) -> Self {
        self.initramfs_path = Some(path.as_ref().to_path_buf());
        self
    }

    pub fn cmdline(mut self, cmdline: impl Into<String>) -> Self {
        self.cmdline = Some(cmdline.into());
        self
    }
}

#[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
pub struct VirtioFs {
    tag: String,
    shared_dir: Option<PathBuf>,
    dax_window_bytes: u64,
    read_only: bool,
    write_allowlist: Option<Vec<PathBuf>>,
    unshare_dir: Option<PathBuf>,
}

#[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
impl VirtioFs {
    pub fn shared(tag: impl Into<String>, path: impl AsRef<Path>) -> Self {
        Self {
            tag: tag.into(),
            shared_dir: Some(path.as_ref().to_path_buf()),
            dax_window_bytes: 0,
            read_only: false,
            write_allowlist: None,
            unshare_dir: None,
        }
    }

    pub fn dax_window_bytes(mut self, bytes: u64) -> Self {
        self.dax_window_bytes = bytes;
        self
    }

    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    pub fn write_allowlist<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.write_allowlist = Some(paths.into_iter().map(Into::into).collect());
        self
    }

    pub fn unshare_dir(mut self, path: impl AsRef<Path>) -> Self {
        self.unshare_dir = Some(path.as_ref().to_path_buf());
        self
    }
}

#[cfg(feature = "net")]
pub struct Network {
    socket: PathBuf,
    mac: [u8; 6],
    dhcp_client: bool,
}

#[cfg(feature = "net")]
impl Network {
    pub fn gvproxy_vfkit(socket: impl AsRef<Path>) -> Self {
        Self {
            socket: socket.as_ref().to_path_buf(),
            mac: [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee],
            dhcp_client: true,
        }
    }
}

pub struct VmBuilder {
    cfg: ContextConfig,
}

impl VmBuilder {
    pub fn new() -> Self {
        let shutdown_efd = if cfg!(target_arch = "aarch64") && cfg!(target_os = "macos") {
            Some(EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap())
        } else {
            None
        };

        Self {
            cfg: ContextConfig {
                krunfw: KrunfwBindings::new(),
                shutdown_efd,
                ..Default::default()
            },
        }
    }

    pub fn resources(&mut self, num_vcpus: u8, ram_mib: u32) -> KrunResult<&mut Self> {
        let mem_size_mib = match ram_mib.try_into() {
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

        if self.cfg.vmr.set_vm_config(&vm_config).is_err() {
            return Err(Error::from_errno(libc::EINVAL));
        }

        Ok(self)
    }

    pub fn console_output(&mut self, filepath: impl AsRef<Path>) -> &mut Self {
        let filepath = filepath.as_ref().to_path_buf();

        #[cfg(feature = "aws-nitro")]
        {
            self.cfg.nitro_console_output = Some(filepath);
        }

        #[cfg(all(not(feature = "aws-nitro"), unix))]
        {
            self.cfg
                .vmr
                .virtio_consoles
                .push(VirtioConsoleConfigMode::OutputFile(filepath));
        }

        self
    }

    pub fn nested_virt(&mut self, enabled: bool) -> &mut Self {
        self.cfg.vmr.nested_enabled = enabled;
        self
    }

    #[cfg(not(feature = "tee"))]
    pub fn kernel(&mut self, kernel: Kernel) -> KrunResult<&mut Self> {
        #[cfg(target_arch = "x86_64")]
        {
            map_kernel_cfg(&mut self.cfg, &kernel.path)?;
            return Ok(self);
        }

        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        {
            let _ = kernel;
            return Err(Error::from_errno(libc::EINVAL));
        }

        #[cfg(target_arch = "aarch64")]
        let (initramfs_path, initramfs_size) = if let Some(initramfs_path) = kernel.initramfs_path {
            let initramfs_size = std::fs::metadata(&initramfs_path)
                .map_err(|_| Error::from_errno(libc::EINVAL))?
                .len();
            (Some(initramfs_path), initramfs_size)
        } else {
            (None, 0)
        };

        #[cfg(target_arch = "aarch64")]
        let external_kernel = ExternalKernel {
            path: kernel.path,
            format: KernelFormat::Raw,
            initramfs_path,
            initramfs_size,
            cmdline: kernel.cmdline,
        };

        #[cfg(target_arch = "aarch64")]
        self.cfg.vmr.set_external_kernel(external_kernel);
        Ok(self)
    }

    pub fn root_pmem(&mut self, file_path: impl AsRef<Path>) -> &mut Self {
        self.cfg.pmem_cfgs.push(PmemDeviceConfig {
            id: "rootfs".to_string(),
            path: file_path.as_ref().to_string_lossy().into_owned(),
            read_only: false,
        });
        self
    }

    #[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
    pub fn virtiofs(&mut self, fs: VirtioFs) -> KrunResult<&mut Self> {
        let shm_size = if fs.dax_window_bytes > 0 {
            Some(
                fs.dax_window_bytes
                    .try_into()
                    .map_err(|_| Error::from_errno(libc::EINVAL))?,
            )
        } else {
            None
        };

        #[cfg(feature = "init-blob")]
        let virtual_entries = if fs.tag == "/dev/root" {
            let mut entries = Vec::new();
            entries.push(init_virtual_entry());
            entries
        } else {
            Vec::new()
        };
        #[cfg(not(feature = "init-blob"))]
        let virtual_entries = Vec::new();

        self.cfg.vmr.add_fs_device(FsDeviceConfig {
            fs_id: fs.tag,
            shared_dir: fs
                .shared_dir
                .map(|path| path.to_string_lossy().into_owned()),
            shm_size,
            read_only: fs.read_only,
            write_allowlist: fs.write_allowlist.map(|paths| Arc::new(RwLock::new(paths))),
            unshare_dir: fs.unshare_dir,
            virtual_entries,
        });
        Ok(self)
    }

    #[cfg(feature = "vhost-user")]
    pub fn vhost_user_virtiofs(
        &mut self,
        tag: impl Into<String>,
        socket_path: impl AsRef<Path>,
    ) -> KrunResult<&mut Self> {
        use vmm::resources::VhostUserDeviceConfig;

        let tag = tag.into();
        let socket_path = socket_path.as_ref().to_string_lossy().into_owned();
        if tag.is_empty() || tag.as_bytes().len() > 36 || socket_path.is_empty() {
            return Err(Error::from_errno(libc::EINVAL));
        }

        let mut config_space = vec![0_u8; 40];
        config_space[..tag.as_bytes().len()].copy_from_slice(tag.as_bytes());
        config_space[36..40].copy_from_slice(&1_u32.to_le_bytes());

        self.cfg.vmr.vhost_user_devices.push(VhostUserDeviceConfig {
            device_type: 26,
            socket_path,
            name: Some(format!("vhost-user-fs-{tag}")),
            num_queues: 2,
            queue_sizes: vec![1024, 1024],
            config_space: Some(config_space),
        });
        Ok(self)
    }

    #[cfg(not(feature = "vhost-user"))]
    pub fn vhost_user_virtiofs(
        &mut self,
        _tag: impl Into<String>,
        _socket_path: impl AsRef<Path>,
    ) -> KrunResult<&mut Self> {
        Err(Error::from_errno(libc::ENOTSUP))
    }

    pub fn vsock_connector(
        &mut self,
        guest_port: u32,
        filepath: impl AsRef<Path>,
    ) -> KrunResult<&mut Self> {
        let filepath = filepath.as_ref().to_path_buf();
        if self.cfg.vsock_config == VsockConfig::Disabled {
            self.cfg.vsock_config = VsockConfig::Explicit {
                tsi_flags: TsiFlags::empty(),
            };
        }
        self.cfg
            .unix_ipc_port_map
            .get_or_insert_with(HashMap::new)
            .insert(guest_port, (filepath, false));
        Ok(self)
    }

    #[cfg(feature = "net")]
    pub fn network(&mut self, network: Network) -> KrunResult<&mut Self> {
        let network_interface_config = NetworkInterfaceConfig {
            iface_id: format!("eth{}", self.cfg.net_index),
            backend: VirtioNetBackend::UnixgramPath(network.socket, true),
            mac: network.mac,
            features: NET_GVPROXY_VFKIT_FEATURES,
        };
        self.cfg.net_index += 1;
        self.cfg
            .vmr
            .add_network_interface(network_interface_config)
            .expect("Failed to create network interface");

        if network.dhcp_client {
            self.cfg.vmr.dhcp_client = true;
        }
        Ok(self)
    }

    pub fn workdir(&mut self, workdir: impl Into<String>) -> &mut Self {
        self.cfg.workdir = Some(workdir.into());
        self
    }

    pub fn exec(
        &mut self,
        exec_path: impl Into<String>,
        argv: &[String],
        envp: &[String],
    ) -> &mut Self {
        self.cfg.exec_path = Some(exec_path.into());
        self.cfg.argv = argv.to_vec();
        self.cfg.env = envp.to_vec();
        self
    }

    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    pub fn deterministic_host_activity_begin() {
        vmm::deterministic_host_activity_begin();
    }

    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    pub fn deterministic_host_activity_end() {
        vmm::deterministic_host_activity_end();
    }

    #[cfg(all(any(target_os = "macos", target_os = "linux"), target_arch = "aarch64"))]
    pub fn restore_from_snapshot(&mut self, path: impl AsRef<Path>) -> KrunResult<&mut Self> {
        self.cfg.snapshot_restore_path = Some(path.as_ref().to_path_buf());
        Ok(self)
    }

    #[cfg(not(all(any(target_os = "macos", target_os = "linux"), target_arch = "aarch64")))]
    pub fn restore_from_snapshot(&mut self, _path: impl AsRef<Path>) -> KrunResult<&mut Self> {
        Err(Error::from_errno(libc::ENOSYS))
    }

    pub fn build(self) -> ConfiguredVm {
        ConfiguredVm {
            cfg: self.cfg,
            handle: VmHandle::new(),
        }
    }
}

impl Default for VmBuilder {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ConfiguredVm {
    cfg: ContextConfig,
    handle: VmHandle,
}

impl ConfiguredVm {
    pub fn handle(&self) -> VmHandle {
        self.handle.clone()
    }

    pub fn start(self) -> KrunResult {
        let handle = self.handle;
        start_enter_context(self.cfg, |vmm| handle.store(vmm))
    }
}

#[derive(Clone)]
pub struct VmHandle {
    vmm: Arc<Mutex<Option<RunningVmm>>>,
}

impl VmHandle {
    fn new() -> Self {
        Self {
            vmm: Arc::new(Mutex::new(None)),
        }
    }

    fn store(&self, vmm: RunningVmm) {
        *self.vmm.lock().unwrap() = Some(vmm);
    }

    fn require_running_vmm(&self) -> KrunResult<RunningVmm> {
        self.vmm.lock().unwrap().clone().ok_or(Error::NotStarted)
    }

    #[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
    pub fn replace_virtiofs_write_allowlist(&self, tag: &str, paths: Vec<PathBuf>) -> KrunResult {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            return if self
                .require_running_vmm()?
                .lock()
                .unwrap()
                .set_virtiofs_write_allowlist(tag, paths)
            {
                Ok(())
            } else {
                Err(Error::from_errno(libc::ENOENT))
            };
        }

        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        {
            let _ = (tag, paths);
            Err(Error::from_errno(libc::ENOENT))
        }
    }

    #[cfg(all(any(target_os = "macos", target_os = "linux"), target_arch = "aarch64"))]
    pub fn snapshot(&self, path: impl AsRef<Path>) -> KrunResult {
        match self
            .require_running_vmm()?
            .lock()
            .unwrap()
            .snapshot(path.as_ref())
        {
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
        let copy_dst_name = copy_dst_name.as_ref();
        if copy_dst_name.is_absolute() || copy_dst_name.components().count() != 1 {
            return Err(Error::from_errno(libc::EINVAL));
        }

        match self
            .require_running_vmm()?
            .lock()
            .unwrap()
            .snapshot_with_file_copy(path.as_ref(), copy_src.as_ref(), copy_dst_name)
        {
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

    #[cfg(not(all(any(target_os = "macos", target_os = "linux"), target_arch = "aarch64")))]
    pub fn snapshot_with_file_copy(
        &self,
        _path: impl AsRef<Path>,
        _copy_src: impl AsRef<Path>,
        _copy_dst_name: impl AsRef<Path>,
    ) -> KrunResult {
        Err(Error::from_errno(libc::ENOSYS))
    }
}

pub fn init_logging(level: LogLevel) -> KrunResult {
    let filter = level.as_filter_str();
    let _ = env_logger::Builder::from_env(Env::default().default_filter_or(filter))
        .format_timestamp_micros()
        .try_init();

    #[cfg(feature = "aws-nitro")]
    {
        // Notify krun-awsnitro to enable debug for log level.
        if matches!(level, LogLevel::Debug | LogLevel::Trace) {
            let mut debug = KRUN_NITRO_DEBUG.lock().unwrap();

            *debug = true;
        }
    }

    Ok(())
}

static KRUNFW: LazyLock<Option<libloading::Library>> = LazyLock::new(|| unsafe {
    std::env::var_os("KRUNFW_PATH")
        .and_then(|path| libloading::Library::new(path).ok())
        .or_else(|| libloading::Library::new(KRUNFW_NAME).ok())
});

struct KrunfwBindings {
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

    fn new() -> Option<Self> {
        Self::load_bindings().ok()
    }
}

#[derive(Default)]
struct ContextConfig {
    krunfw: Option<KrunfwBindings>,
    vmr: VmResources,
    workdir: Option<String>,
    exec_path: Option<String>,
    env: Vec<String>,
    argv: Vec<String>,
    #[cfg(feature = "net")]
    net_index: u8,
    vsock_config: VsockConfig,
    pmem_cfgs: Vec<PmemDeviceConfig>,
    #[cfg(feature = "tee")]
    tee_config_file: Option<PathBuf>,
    unix_ipc_port_map: Option<HashMap<u32, (PathBuf, bool)>>,
    shutdown_efd: Option<EventFd>,
    #[cfg(feature = "aws-nitro")]
    nitro_console_output: Option<PathBuf>,
    /// If set, `krun_start_enter` will restore from this snapshot directory
    /// instead of doing a fresh boot.
    #[cfg(all(any(target_os = "macos", target_os = "linux"), target_arch = "aarch64"))]
    snapshot_restore_path: Option<PathBuf>,
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

        if ctx.env.is_empty() {
            error!("execution env not specified");
            return Err(-libc::EINVAL);
        }
        let exec_env = ctx.env.join(" ");

        if ctx.argv.is_empty() {
            error!("execution args not specified");
            return Err(-libc::EINVAL);
        }
        let exec_args = ctx.argv.join(" ");

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

        let Some(output_path) = ctx.nitro_console_output else {
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

#[cfg(feature = "net")]
const NET_FEATURE_CSUM: u32 = 1 << 0;
#[cfg(feature = "net")]
const NET_FEATURE_GUEST_CSUM: u32 = 1 << 1;
#[cfg(feature = "net")]
const NET_FEATURE_GUEST_TSO4: u32 = 1 << 7;
#[cfg(feature = "net")]
const NET_FEATURE_GUEST_UFO: u32 = 1 << 10;
#[cfg(feature = "net")]
const NET_FEATURE_HOST_TSO4: u32 = 1 << 11;
#[cfg(feature = "net")]
const NET_FEATURE_HOST_UFO: u32 = 1 << 14;
#[cfg(feature = "net")]
const NET_GVPROXY_VFKIT_FEATURES: u32 = NET_FEATURE_CSUM
    | NET_FEATURE_GUEST_CSUM
    | NET_FEATURE_GUEST_TSO4
    | NET_FEATURE_GUEST_UFO
    | NET_FEATURE_HOST_TSO4
    | NET_FEATURE_HOST_UFO;

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
    if let Some(tee_config) = ctx_cfg.tee_config_file.clone() {
        if let Err(e) = ctx_cfg.vmr.set_tee_config(tee_config) {
            error!("Error setting up TEE config: {e:?}");
            return Err(Error::from_errno(libc::EINVAL));
        }
    } else {
        error!("Missing TEE config file");
        return Err(Error::from_errno(libc::EINVAL));
    }

    let exec_path_env = ctx_cfg
        .exec_path
        .as_deref()
        .map(|exec_path| format!("KRUN_INIT={exec_path}"))
        .unwrap_or_default();
    let workdir_env = ctx_cfg
        .workdir
        .as_deref()
        .map(|workdir| format!("KRUN_WORKDIR={workdir}"))
        .unwrap_or_default();
    let guest_env = ctx_cfg.env.join(" ");
    let argv = ctx_cfg.argv.join(" ");

    let kernel_cmdline = KernelCmdlineConfig {
        prolog: Some(format!("{DEFAULT_KERNEL_CMDLINE} init={INIT_PATH}")),
        krun_env: Some(format!(" {exec_path_env} {workdir_env} {guest_env}")),
        epilog: Some(format!(" -- {argv}")),
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
    }
    vmm::timing_event("start_enter.vsock.configured");

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
