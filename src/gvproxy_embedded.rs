#[cfg(target_os = "macos")]
use std::{
    ffi::{CStr, CString, c_char, c_int, c_longlong},
    path::Path,
};

#[cfg(target_os = "macos")]
use anyhow::{Context, Result, bail};

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn lnx_gvproxy_start(
        vfkit_endpoint: *const c_char,
        log_path: *const c_char,
        ssh_port: c_int,
    ) -> c_longlong;
    fn lnx_gvproxy_stop(id: c_longlong);
    fn lnx_gvproxy_last_error() -> *const c_char;
}

#[cfg(target_os = "macos")]
pub struct EmbeddedGvproxy {
    id: c_longlong,
}

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
impl Drop for EmbeddedGvproxy {
    fn drop(&mut self) {
        unsafe {
            lnx_gvproxy_stop(self.id);
        }
    }
}

#[cfg(target_os = "macos")]
fn last_error() -> String {
    let ptr = unsafe { lnx_gvproxy_last_error() };
    if ptr.is_null() {
        return "unknown error".to_string();
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}
