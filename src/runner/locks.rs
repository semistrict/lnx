use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use lnx_protocol::PROTOCOL_VERSION;

use super::json_escape;

pub(crate) fn owner_pid_from_lock(lock_path: &Path) -> Option<libc::pid_t> {
    let pid = fs::read_to_string(lock_path.join("owner.pid")).ok()?;
    let pid = pid.trim().parse::<libc::pid_t>().ok()?;
    process_alive(pid).then_some(pid)
}

pub(crate) fn signal_process_group(pid: libc::pid_t, signal: libc::c_int) -> Result<()> {
    if pid <= 0 {
        bail!("invalid owner pid: {pid}");
    }
    let pgid = -pid;
    let rc = unsafe { libc::kill(pgid, signal) };
    if rc == 0 {
        return Ok(());
    }
    let group_error = std::io::Error::last_os_error();
    let rc = unsafe { libc::kill(pid, signal) };
    if rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(group_error).with_context(|| format!("signal process group {pid}"))
}

pub(crate) struct BootstrapLock {
    path: PathBuf,
}

pub(crate) struct OwnerStartLock {
    path: PathBuf,
}

pub(crate) enum BootstrapOutcome {
    Lock(BootstrapLock),
    Status(i32),
}

pub(crate) enum OwnerStartOutcome {
    Lock(OwnerStartLock),
    Status(i32),
}

impl BootstrapLock {
    pub(crate) fn try_acquire(path: &Path) -> Result<Option<Self>> {
        match fs::create_dir(path) {
            Ok(()) => {
                write_owner_lease(path)?;
                Ok(Some(Self {
                    path: path.to_path_buf(),
                }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if reclaim_stale_lock_dir(path, bootstrap_lock_is_stale, write_owner_lease)? {
                    return Ok(Some(Self {
                        path: path.to_path_buf(),
                    }));
                }
                Ok(None)
            }
            Err(e) => Err(e).with_context(|| format!("create {}", path.display())),
        }
    }
}

/// Serializes the stale-check/remove/recreate sequence for a directory lock.
/// The guard file lives beside the lock dir and is never removed; flock on it
/// ensures only one process acts on a stale lock at a time.
pub(crate) fn reclaim_stale_lock_dir(
    path: &Path,
    is_stale: impl Fn(&Path) -> Result<bool>,
    write_lease: impl Fn(&Path) -> Result<()>,
) -> Result<bool> {
    let guard_path = {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".guard");
        path.with_file_name(name)
    };
    let guard = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&guard_path)
        .with_context(|| format!("open {}", guard_path.display()))?;
    lock_file(&guard).with_context(|| format!("lock {}", guard_path.display()))?;
    let result = (|| {
        // Re-check under the guard: the previous holder may have already
        // reclaimed and now legitimately owns a fresh lock dir.
        match is_stale(path) {
            Ok(false) => return Ok(false),
            Ok(true) => {}
            Err(_) if !path.exists() => {
                // The dir vanished between the caller's observation and this
                // guarded re-check (e.g. the previous holder's Drop ran).
                // Fall through to try creating it fresh.
            }
            Err(e) => return Err(e),
        }
        let _ = fs::remove_dir_all(path);
        match fs::create_dir(path) {
            Ok(()) => {
                write_lease(path)?;
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e).with_context(|| format!("create {}", path.display())),
        }
    })();
    let _ = unlock_file(&guard);
    result
}

impl Drop for BootstrapLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.path.join("owner.pid"));
        let _ = fs::remove_file(self.path.join("owner.json"));
        let _ = fs::remove_dir(&self.path);
    }
}

impl OwnerStartLock {
    pub(crate) fn try_acquire(path: &Path) -> Result<Option<Self>> {
        match fs::create_dir(path) {
            Ok(()) => {
                write_pid_file(path, "starter.pid")?;
                Ok(Some(Self {
                    path: path.to_path_buf(),
                }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if reclaim_stale_lock_dir(path, owner_start_lock_is_stale, |p| {
                    write_pid_file(p, "starter.pid")
                })? {
                    return Ok(Some(Self {
                        path: path.to_path_buf(),
                    }));
                }
                Ok(None)
            }
            Err(e) => Err(e).with_context(|| format!("create {}", path.display())),
        }
    }
}

impl Drop for OwnerStartLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.path.join("starter.pid"));
        let _ = fs::remove_dir(&self.path);
    }
}

pub(crate) fn write_pid_file(path: &Path, name: &str) -> Result<()> {
    let file = path.join(name);
    fs::write(&file, std::process::id().to_string())
        .with_context(|| format!("write {}", file.display()))
}

pub(crate) fn write_owner_lease(path: &Path) -> Result<()> {
    let pid = std::process::id();
    write_pid_file(path, "owner.pid")?;
    let exe = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| String::new());
    let lease = format!(
        "{{\"pid\":{pid},\"protocol_version\":{},\"agent_source_stamp\":\"{}\",\"binary_path\":\"{}\"}}\n",
        PROTOCOL_VERSION,
        json_escape(env!("LNX_AGENT_SOURCE_STAMP")),
        json_escape(&exe)
    );
    fs::write(path.join("owner.json"), lease)
        .with_context(|| format!("write {}", path.join("owner.json").display()))?;
    Ok(())
}

pub(crate) fn bootstrap_lock_is_stale(path: &Path) -> Result<bool> {
    let owner_pid = path.join("owner.pid");
    if let Ok(pid) = fs::read_to_string(&owner_pid) {
        if let Ok(pid) = pid.trim().parse::<libc::pid_t>() {
            return Ok(!process_alive(pid));
        }
        return Ok(true);
    }

    let modified = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .modified()
        .with_context(|| format!("stat modified time {}", path.display()))?;
    Ok(modified.elapsed().unwrap_or_default() > Duration::from_secs(10))
}

pub(crate) fn owner_start_lock_is_stale(path: &Path) -> Result<bool> {
    let starter_pid = path.join("starter.pid");
    if let Ok(pid) = fs::read_to_string(&starter_pid) {
        if let Ok(pid) = pid.trim().parse::<libc::pid_t>() {
            return Ok(!process_alive(pid));
        }
        return Ok(true);
    }

    let modified = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .modified()
        .with_context(|| format!("stat modified time {}", path.display()))?;
    Ok(modified.elapsed().unwrap_or_default() > Duration::from_secs(10))
}

pub(crate) fn process_alive(pid: libc::pid_t) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe {
        libc::kill(pid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

pub(crate) fn lock_file(file: &fs::File) -> std::io::Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub(crate) fn unlock_file(file: &fs::File) -> std::io::Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
