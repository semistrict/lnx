use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use lnx_protocol::PROTOCOL_VERSION;

use super::json_escape;

pub(crate) fn owner_pid_from_lock(lock_path: &Path) -> Option<libc::pid_t> {
    let pid = recorded_owner_pid_from_lock(lock_path).ok().flatten()?;
    process_alive(pid).then_some(pid)
}

pub(crate) fn recorded_owner_pid_from_lock(lock_path: &Path) -> Result<Option<libc::pid_t>> {
    let path = lock_path.join("owner.pid");
    let value = match fs::read_to_string(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let pid = value
        .trim()
        .parse::<libc::pid_t>()
        .with_context(|| format!("parse {}", path.display()))?;
    if pid <= 0 {
        bail!("invalid owner pid {pid} in {}", path.display());
    }
    Ok(Some(pid))
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

pub(crate) fn signal_process(pid: libc::pid_t, signal: libc::c_int) -> Result<()> {
    if pid <= 0 {
        bail!("invalid owner pid: {pid}");
    }
    let rc = unsafe { libc::kill(pid, signal) };
    if rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(std::io::Error::last_os_error()).with_context(|| format!("signal process {pid}"))
}

pub(crate) struct BootstrapLock {
    path: PathBuf,
}

pub(crate) struct OwnerStartLock {
    path: PathBuf,
}

pub(crate) struct InstanceStateLock {
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
    #[cfg(test)]
    pub(crate) fn try_acquire(path: &Path) -> Result<Option<Self>> {
        Self::try_acquire_validated(path, || Ok(()))
    }

    pub(crate) fn try_acquire_validated(
        path: &Path,
        validate: impl FnOnce() -> Result<()>,
    ) -> Result<Option<Self>> {
        try_acquire_lock_dir(path, bootstrap_lock_is_stale, validate, write_owner_lease).map(
            |acquired| {
                acquired.then(|| Self {
                    path: path.to_path_buf(),
                })
            },
        )
    }
}

fn lock_dir_guard_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".guard");
    path.with_file_name(name)
}

pub(crate) fn with_lock_dir_guard<T>(path: &Path, action: impl FnOnce() -> Result<T>) -> Result<T> {
    with_lock_dir_guard_inner(path, true, action)
}

pub(crate) fn with_existing_lock_dir_guard<T>(
    path: &Path,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    with_lock_dir_guard_inner(path, false, action)
}

fn with_lock_dir_guard_inner<T>(
    path: &Path,
    create_parent: bool,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let guard_path = lock_dir_guard_path(path);
    if create_parent && let Some(parent) = guard_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let guard = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&guard_path)
        .with_context(|| format!("open {}", guard_path.display()))?;
    lock_file_with_timeout(&guard, Duration::from_secs(5))
        .with_context(|| format!("lock {}", guard_path.display()))?;
    let result = action();
    let _ = unlock_file(&guard);
    result
}

fn lock_file_with_timeout(file: &fs::File, timeout: Duration) -> std::io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for lock-directory guard",
            ));
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        let would_block = matches!(
            error.raw_os_error(),
            Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK
        );
        if !would_block {
            return Err(error);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn try_acquire_lock_dir(
    path: &Path,
    is_stale: impl Fn(&Path) -> Result<bool>,
    validate: impl FnOnce() -> Result<()>,
    write_lease: impl Fn(&Path) -> Result<()>,
) -> Result<bool> {
    with_lock_dir_guard(path, || {
        if path.exists() && !is_stale(path)? {
            return Ok(false);
        }
        validate()?;
        if path.exists() {
            fs::remove_dir_all(path)
                .with_context(|| format!("remove stale lock {}", path.display()))?;
        }
        fs::create_dir(path).with_context(|| format!("create {}", path.display()))?;
        if let Err(error) = write_lease(path) {
            let _ = fs::remove_dir_all(path);
            return Err(error);
        }
        Ok(true)
    })
}

/// Removes a stale directory lock without claiming it.
///
/// This uses the same sidecar guard as stale reclamation and re-checks the
/// lease while holding that guard. A caller may therefore act on an earlier
/// stale observation without deleting a fresh lease installed in the
/// meantime.
#[cfg(test)]
pub(crate) fn remove_stale_lock_dir(
    path: &Path,
    is_stale: impl Fn(&Path) -> Result<bool>,
) -> Result<bool> {
    with_lock_dir_guard(path, || {
        if !path.exists() {
            return Ok(false);
        }
        if !is_stale(path)? {
            return Ok(false);
        }
        fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))?;
        Ok(true)
    })
}

pub(crate) fn with_unowned_bootstrap_lock<T>(
    path: &Path,
    action: impl FnOnce() -> Result<T>,
) -> Result<Option<T>> {
    with_lock_dir_guard(path, || {
        if path.exists() && !bootstrap_lock_is_stale(path)? {
            return Ok(None);
        }
        let result = action()?;
        if path.exists() {
            fs::remove_dir_all(path)
                .with_context(|| format!("remove stale lock {}", path.display()))?;
        }
        Ok(Some(result))
    })
}

impl Drop for BootstrapLock {
    fn drop(&mut self) {
        release_lock_dir(&self.path, "owner.pid", &["owner.pid", "owner.json"]);
    }
}

impl InstanceStateLock {
    pub(crate) fn try_acquire_validated(
        path: &Path,
        validate: impl FnOnce() -> Result<()>,
    ) -> Result<Option<Self>> {
        try_acquire_lock_dir(path, bootstrap_lock_is_stale, validate, |path| {
            write_pid_file(path, "maintenance.pid")
        })
        .map(|acquired| {
            acquired.then(|| Self {
                path: path.to_path_buf(),
            })
        })
    }
}

impl Drop for InstanceStateLock {
    fn drop(&mut self) {
        release_lock_dir(&self.path, "maintenance.pid", &["maintenance.pid"]);
    }
}

impl OwnerStartLock {
    pub(crate) fn try_acquire(path: &Path) -> Result<Option<Self>> {
        try_acquire_lock_dir(
            path,
            owner_start_lock_is_stale,
            || Ok(()),
            |p| write_pid_file(p, "starter.pid"),
        )
        .map(|acquired| {
            acquired.then(|| Self {
                path: path.to_path_buf(),
            })
        })
    }
}

impl Drop for OwnerStartLock {
    fn drop(&mut self) {
        release_lock_dir(&self.path, "starter.pid", &["starter.pid"]);
    }
}

fn release_lock_dir(path: &Path, pid_file: &str, lease_files: &[&str]) {
    if !path.exists() {
        return;
    }
    let expected_pid = std::process::id() as libc::pid_t;
    let _ = with_existing_lock_dir_guard(path, || {
        let recorded_pid = fs::read_to_string(path.join(pid_file))
            .ok()
            .and_then(|pid| pid.trim().parse::<libc::pid_t>().ok());
        if recorded_pid != Some(expected_pid) {
            return Ok(());
        }
        for lease_file in lease_files {
            let _ = fs::remove_file(path.join(lease_file));
        }
        let _ = fs::remove_dir(path);
        Ok(())
    });
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

    let maintenance_pid = path.join("maintenance.pid");
    if let Ok(pid) = fs::read_to_string(&maintenance_pid) {
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

pub(crate) fn recorded_maintenance_pid_from_lock(path: &Path) -> Result<Option<libc::pid_t>> {
    let pid_path = path.join("maintenance.pid");
    let value = match fs::read_to_string(&pid_path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", pid_path.display())),
    };
    let pid = value
        .trim()
        .parse::<libc::pid_t>()
        .with_context(|| format!("parse {}", pid_path.display()))?;
    if pid <= 0 {
        bail!("invalid maintenance pid {pid} in {}", pid_path.display());
    }
    Ok(Some(pid))
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
