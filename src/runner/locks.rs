//! Instance locks.
//!
//! Every lock is an `flock(2)` on a file inside the instance directory, so the
//! kernel releases it when the holder exits, however it exits. A holder
//! records who it is (a [`Lease`]) inside the lock file; the record is only
//! trusted while the lock is held, and it names the holder by pid *and*
//! process start time so a recycled pid is never mistaken for the holder.
//!
//! Two locks protect an instance:
//!
//! - `instance.lock` is held exclusively by whoever may mutate the instance's
//!   state: the VM owner for its whole lifetime, or a maintenance command
//!   (snapshot clear, checkpoint delete, instance delete, state copy) while
//!   the instance is stopped. Acquiring and inspecting it happen under
//!   `instance.lock.guard`, a short-lived lock that makes "inspect, then act"
//!   sequences atomic with respect to other acquirers.
//! - `owner-start.lock` serializes clients that spawn a new VM owner.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use lnx_protocol::PROTOCOL_VERSION;
use serde::{Deserialize, Serialize};

use crate::paths::Layout;

pub(crate) const INSTANCE_LOCK: &str = "instance.lock";
pub(crate) const INSTANCE_LOCK_GUARD: &str = "instance.lock.guard";
pub(crate) const OWNER_START_LOCK: &str = "owner-start.lock";
pub(crate) const LOCK_FILES: [&str; 3] = [INSTANCE_LOCK, INSTANCE_LOCK_GUARD, OWNER_START_LOCK];

const GUARD_TIMEOUT: Duration = Duration::from_secs(5);

/// A process named so that it cannot be confused with a later process that
/// happens to reuse its pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcessIdentity {
    pub(crate) pid: libc::pid_t,
    /// Opaque, platform-specific start time of the process.
    pub(crate) started: u64,
}

impl ProcessIdentity {
    pub(crate) fn current() -> Self {
        static CURRENT: OnceLock<ProcessIdentity> = OnceLock::new();
        *CURRENT.get_or_init(|| {
            let pid = std::process::id() as libc::pid_t;
            Self::of(pid).unwrap_or(Self { pid, started: 0 })
        })
    }

    /// The identity of the process currently running as `pid`, if any.
    pub(crate) fn of(pid: libc::pid_t) -> Option<Self> {
        if pid <= 0 {
            return None;
        }
        process_start_time(pid).map(|started| Self { pid, started })
    }

    pub(crate) fn is_running(&self) -> bool {
        Self::of(self.pid).is_some_and(|current| current == *self)
    }

    /// Signals the process group led by this process (falling back to the
    /// process alone), unless the process has already exited.
    pub(crate) fn signal_group(&self, signal: libc::c_int) -> Result<()> {
        if !self.is_running() {
            return Ok(());
        }
        signal_process_group(self.pid, signal)
    }

    pub(crate) fn signal(&self, signal: libc::c_int) -> Result<()> {
        if !self.is_running() {
            return Ok(());
        }
        signal_process(self.pid, signal)
    }
}

#[cfg(target_os = "macos")]
fn process_start_time(pid: libc::pid_t) -> Option<u64> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (written == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

#[cfg(target_os = "linux")]
fn process_start_time(pid: libc::pid_t) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name (field 2) may contain spaces and parentheses; the
    // remaining fields follow the last ')'. Start time is field 22.
    let fields = stat.get(stat.rfind(')')? + 1..)?;
    fields.split_whitespace().nth(19)?.parse().ok()
}

pub(crate) fn signal_process_group(pid: libc::pid_t, signal: libc::c_int) -> Result<()> {
    if pid <= 0 {
        bail!("invalid owner pid: {pid}");
    }
    let rc = unsafe { libc::kill(-pid, signal) };
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LeaseRole {
    /// A VM owner process; holds the instance lock for its whole lifetime.
    Owner,
    /// A command operating on the stopped instance's state.
    Maintenance,
    /// A client spawning a new VM owner.
    Starter,
}

/// Who holds a lock, as recorded by the holder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Lease {
    pub(crate) role: LeaseRole,
    pub(crate) process: ProcessIdentity,
    pub(crate) protocol_version: u16,
    pub(crate) agent_source_stamp: String,
    pub(crate) binary_path: String,
}

impl Lease {
    fn for_current_process(role: LeaseRole) -> Self {
        Self {
            role,
            process: ProcessIdentity::current(),
            protocol_version: PROTOCOL_VERSION,
            agent_source_stamp: env!("LNX_AGENT_SOURCE_STAMP").to_string(),
            binary_path: std::env::current_exe()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
        }
    }

    pub(crate) fn pid(&self) -> libc::pid_t {
        self.process.pid
    }
}

/// An exclusive `flock` on a lock file, released when dropped.
#[derive(Debug)]
struct HeldLock {
    file: File,
    path: PathBuf,
    /// Whether releasing the lock erases the lease in it.
    clear_lease_on_release: bool,
}

impl HeldLock {
    fn try_acquire(path: &Path) -> Result<Option<Self>> {
        let file = open_lock_file(path)?;
        match flock(&file, libc::LOCK_EX | libc::LOCK_NB)? {
            true => Ok(Some(Self {
                file,
                path: path.to_path_buf(),
                clear_lease_on_release: true,
            })),
            false => Ok(None),
        }
    }

    fn acquire_with_timeout(path: &Path, timeout: Duration) -> Result<Self> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(lock) = Self::try_acquire(path)? {
                return Ok(lock);
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for lock {}", path.display());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn write_lease(&mut self, lease: &Lease) -> Result<()> {
        let mut content = serde_json::to_vec(lease).context("encode lease")?;
        content.push(b'\n');
        self.file
            .set_len(0)
            .and_then(|()| self.file.seek(SeekFrom::Start(0)).map(drop))
            .and_then(|()| self.file.write_all(&content))
            .with_context(|| format!("write lease {}", self.path.display()))
    }

    fn read_lease(&mut self) -> Result<Option<Lease>> {
        read_lease_from(&mut self.file, &self.path)
    }
}

impl Drop for HeldLock {
    fn drop(&mut self) {
        // A cleanly released lock carries no lease; a lease left behind
        // therefore names a holder that died while holding the lock.
        if self.clear_lease_on_release {
            let _ = self.file.set_len(0);
        }
    }
}

fn open_lock_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open lock {}", path.display()))
}

/// Returns whether the lock was taken; `false` means another holder has it.
fn flock(file: &File, operation: libc::c_int) -> Result<bool> {
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => return Ok(false),
            _ => return Err(error).context("flock"),
        }
    }
}

fn read_lease_from(file: &mut File, path: &Path) -> Result<Option<Lease>> {
    let mut content = String::new();
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.read_to_string(&mut content))
        .with_context(|| format!("read lease {}", path.display()))?;
    if content.trim().is_empty() {
        return Ok(None);
    }
    // A holder killed mid-write leaves a torn record; it names nobody.
    Ok(serde_json::from_str(content.trim()).ok())
}

pub(crate) fn instance_lock_path(layout: &Layout) -> PathBuf {
    layout.instance_dir.join(INSTANCE_LOCK)
}

fn instance_guard_path(layout: &Layout) -> PathBuf {
    layout.instance_dir.join(INSTANCE_LOCK_GUARD)
}

pub(crate) fn owner_start_lock_path(layout: &Layout) -> PathBuf {
    layout.instance_dir.join(OWNER_START_LOCK)
}

/// The instance lock as seen by someone who does not hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InstanceLockState {
    /// Nobody holds the lock. `stale` is the lease a holder left behind when
    /// it died without releasing cleanly, if any.
    Free { stale: Option<Lease> },
    /// A live process holds the lock. `lease` is `None` only in the instant
    /// between a holder taking the lock and recording itself.
    Held { lease: Option<Lease> },
}

impl InstanceLockState {
    pub(crate) fn live_owner(&self) -> Option<&Lease> {
        match self {
            Self::Held { lease: Some(lease) } if lease.role == LeaseRole::Owner => Some(lease),
            _ => None,
        }
    }

    pub(crate) fn is_held(&self) -> bool {
        matches!(self, Self::Held { .. })
    }

    /// The lease of a holder that died while holding the lock.
    pub(crate) fn stale_lease(&self) -> Option<&Lease> {
        match self {
            Self::Free { stale } => stale.as_ref(),
            Self::Held { .. } => None,
        }
    }
}

/// Runs `action` while no other process can acquire or release-and-reacquire
/// the instance lock, passing it the lock's current state.
///
/// Inspecting an instance never creates it: without an instance directory
/// there is nothing to guard and nobody can hold the lock.
pub(crate) fn with_instance_guard<T>(
    layout: &Layout,
    action: impl FnOnce(&InstanceLockState) -> Result<T>,
) -> Result<T> {
    if !layout.instance_dir.exists() {
        return action(&InstanceLockState::Free { stale: None });
    }
    let _guard = HeldLock::acquire_with_timeout(&instance_guard_path(layout), GUARD_TIMEOUT)?;
    let state = probe_instance_lock(layout)?;
    action(&state)
}

/// Must only be called while holding the instance guard, so that the probe
/// cannot make a concurrent acquirer see the lock as busy.
fn probe_instance_lock(layout: &Layout) -> Result<InstanceLockState> {
    let path = instance_lock_path(layout);
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(InstanceLockState::Free { stale: None });
        }
        Err(error) => return Err(error).with_context(|| format!("open lock {}", path.display())),
    };
    let free = flock(&file, libc::LOCK_SH | libc::LOCK_NB)?;
    let lease = read_lease_from(&mut file, &path)?;
    // Dropping `file` releases the probe's shared lock.
    Ok(if free {
        InstanceLockState::Free { stale: lease }
    } else {
        InstanceLockState::Held { lease }
    })
}

pub(crate) fn instance_lock_state(layout: &Layout) -> Result<InstanceLockState> {
    with_instance_guard(layout, |state| Ok(state.clone()))
}

/// The live VM owner of an instance, if one is running.
pub(crate) fn live_owner(layout: &Layout) -> Option<Lease> {
    instance_lock_state(layout)
        .ok()
        .and_then(|state| state.live_owner().cloned())
}

/// Exclusive ownership of an instance's mutable state.
#[derive(Debug)]
pub(crate) struct InstanceLock {
    _held: HeldLock,
}

impl InstanceLock {
    /// Takes the instance lock for `role` unless another live process holds
    /// it. `validate` runs after the lock is taken but before the new lease
    /// replaces any stale one, so it can inspect what a dead holder left.
    /// When it fails, the lock is released again and the error returned.
    pub(crate) fn try_acquire(
        layout: &Layout,
        role: LeaseRole,
        validate: impl FnOnce(Option<&Lease>) -> Result<()>,
    ) -> Result<Option<Self>> {
        let _guard = HeldLock::acquire_with_timeout(&instance_guard_path(layout), GUARD_TIMEOUT)?;
        let Some(mut held) = HeldLock::try_acquire(&instance_lock_path(layout))? else {
            return Ok(None);
        };
        let stale = held.read_lease()?;
        if let Err(error) = validate(stale.as_ref()) {
            // Leave the stale lease in place for whoever acts on the error.
            held.clear_lease_on_release = false;
            return Err(error);
        }
        held.write_lease(&Lease::for_current_process(role))?;
        Ok(Some(Self { _held: held }))
    }
}

/// Serializes clients that spawn a VM owner for one instance.
#[derive(Debug)]
pub(crate) struct OwnerStartLock {
    _held: HeldLock,
}

impl OwnerStartLock {
    pub(crate) fn try_acquire(layout: &Layout) -> Result<Option<Self>> {
        let Some(mut held) = HeldLock::try_acquire(&owner_start_lock_path(layout))? else {
            return Ok(None);
        };
        held.write_lease(&Lease::for_current_process(LeaseRole::Starter))?;
        Ok(Some(Self { _held: held }))
    }
}

/// An advisory lock on an already-open file, for single-file critical
/// sections such as the timing-log state.
pub(crate) fn lock_file(file: &File) -> std::io::Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub(crate) fn unlock_file(file: &File) -> std::io::Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::process::{Child, Command, Stdio};

    use super::*;

    /// Takes the instance lock as a VM owner in this process.
    pub(crate) fn hold_as_owner(layout: &Layout) -> InstanceLock {
        InstanceLock::try_acquire(layout, LeaseRole::Owner, |_| Ok(()))
            .expect("acquire instance lock")
            .expect("instance lock is free")
    }

    pub(crate) fn lease_for(role: LeaseRole, process: ProcessIdentity) -> Lease {
        Lease {
            process,
            ..Lease::for_current_process(role)
        }
    }

    /// Records `lease` in the instance lock file without taking the lock,
    /// as a holder that died while holding the lock leaves it, or as a live
    /// holder that has not finished recording itself would.
    pub(crate) fn write_instance_lease(layout: &Layout, lease: &Lease) {
        let path = instance_lock_path(layout);
        let mut file = open_lock_file(&path).expect("open instance lock");
        let mut content = serde_json::to_vec(lease).expect("encode lease");
        content.push(b'\n');
        file.set_len(0).expect("truncate lease");
        file.write_all(&content).expect("write lease");
    }

    /// The identity of a process that has exited and been reaped.
    pub(crate) fn exited_process() -> ProcessIdentity {
        let mut child = Command::new("/bin/sleep")
            .arg("60")
            .spawn()
            .expect("spawn short-lived process");
        let identity = ProcessIdentity::of(child.id() as libc::pid_t).expect("child identity");
        child.kill().expect("kill short-lived process");
        child.wait().expect("reap short-lived process");
        identity
    }

    /// A separate process holding the instance lock, recorded as `role`.
    /// `term_handler` is the Perl body of its SIGTERM handler; the kernel
    /// releases the lock when the process exits.
    pub(crate) struct ForeignHolder {
        pub(crate) child: Child,
        pub(crate) process: ProcessIdentity,
    }

    pub(crate) fn spawn_foreign_holder(
        layout: &Layout,
        role: LeaseRole,
        term_handler: &str,
    ) -> ForeignHolder {
        let path = instance_lock_path(layout);
        drop(open_lock_file(&path).expect("create instance lock"));
        let script = format!(
            r#"use Fcntl qw(:flock);
               open(my $lock, "+<", $ARGV[0]) or die "open: $!";
               flock($lock, LOCK_EX) or die "flock: $!";
               $SIG{{TERM}} = sub {{ {term_handler} }};
               $| = 1; print "locked\n";
               sleep 1 while 1;"#
        );
        let mut child = Command::new("/usr/bin/perl")
            .arg("-e")
            .arg(script)
            .arg(&path)
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .expect("spawn lock holder");
        let mut ready = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.as_mut().expect("holder stdout")),
            &mut ready,
        )
        .expect("read holder readiness");
        assert_eq!(ready, "locked\n");
        let process = ProcessIdentity::of(child.id() as libc::pid_t).expect("holder identity");
        write_instance_lease(layout, &lease_for(role, process));
        ForeignHolder { child, process }
    }

    use std::os::unix::process::CommandExt;
}
