#[cfg(target_os = "macos")]
mod platform {
    use std::{
        ffi::{CStr, CString, c_char, c_int, c_longlong},
        path::Path,
    };

    use anyhow::{Context, Result, bail};

    unsafe extern "C" {
        fn lnx_gvproxy_start(
            vfkit_endpoint: *const c_char,
            log_path: *const c_char,
            ssh_port: c_int,
        ) -> c_longlong;
        fn lnx_gvproxy_stop(id: c_longlong);
        fn lnx_gvproxy_last_error() -> *const c_char;
    }

    pub struct EmbeddedGvproxy {
        id: c_longlong,
    }

    impl EmbeddedGvproxy {
        pub fn start(socket: &Path, log: &Path, ssh_port: u16) -> Result<Self> {
            let endpoint = CString::new(format!("unixgram:{}", socket.display()))
                .context("embedded gvproxy endpoint contains nul")?;
            let log = CString::new(log.as_os_str().as_encoded_bytes())
                .context("embedded gvproxy log path contains nul")?;
            let id = unsafe { lnx_gvproxy_start(endpoint.as_ptr(), log.as_ptr(), ssh_port.into()) };
            if id < 0 {
                bail!("embedded gvproxy failed to start: {}", last_error());
            }
            Ok(Self { id })
        }
    }

    impl Drop for EmbeddedGvproxy {
        fn drop(&mut self) {
            unsafe {
                lnx_gvproxy_stop(self.id);
            }
        }
    }

    fn last_error() -> String {
        let ptr = unsafe { lnx_gvproxy_last_error() };
        if ptr.is_null() {
            return "unknown error".to_string();
        }
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
    };

    use anyhow::{Context, Result};

    const GVPROXY_BRIDGE: &[u8] = include_bytes!(env!("LNX_GVPROXY_BRIDGE"));

    pub struct EmbeddedGvproxy {
        child: Child,
        executable: PathBuf,
    }

    impl EmbeddedGvproxy {
        pub fn start(socket: &Path, log: &Path, ssh_port: u16) -> Result<Self> {
            let executable = socket.with_file_name("lnx-gvproxy-bridge");
            fs::write(&executable, GVPROXY_BRIDGE)
                .with_context(|| format!("write {}", executable.display()))?;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
                .with_context(|| format!("chmod {}", executable.display()))?;

            let child = Command::new(&executable)
                .arg("--listen-vfkit")
                .arg(format!("unixgram:{}", socket.display()))
                .arg("--log")
                .arg(log)
                .arg("--ssh-port")
                .arg(ssh_port.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .with_context(|| format!("start {}", executable.display()))?;

            Ok(Self { child, executable })
        }
    }

    impl Drop for EmbeddedGvproxy {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = fs::remove_file(&self.executable);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use platform::EmbeddedGvproxy;
