use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use crate::paths::Layout;

use super::{
    DETERMINISTIC_CLOCK_STATE, LAUNCH_METADATA, RestoreRefused, RunLog, TimingLog,
    clone_or_copy_file, log_value, remove_path_if_exists, system_time_unix_nanos, unix_nanos,
};

// Accepted vmstate.bin container version. Source backend lives in the META
// section, not in the header version.
pub(crate) const SNAPSHOT_VMSTATE_VERSION: u32 = 4;
pub(crate) const RESTORE_WORK_SNAPSHOT: &str = ".restore-work";
pub(crate) const RESTORE_WORK_ACTIVE_MARKER: &str = ".restore-work.active";
pub(crate) const FINAL_SNAPSHOT_OUTCOME: &str = "final-snapshot.outcome";
pub(crate) const SNAPSHOT_LIFECYCLE_META: &str = "snapshot.meta";
pub(crate) const SNAPSHOT_RESTORE_FILES: &[&str] = &[
    "vmstate.bin",
    "pages.img",
    "rootfs.ext4",
    "initramfs.stamp",
    LAUNCH_METADATA,
    "deterministic.stamp",
    DETERMINISTIC_CLOCK_STATE,
    SNAPSHOT_LIFECYCLE_META,
];

pub(crate) struct SnapshotVmConfig {
    #[cfg_attr(
        any(not(all(target_os = "linux", target_arch = "aarch64")), not(test)),
        allow(dead_code)
    )]
    pub(crate) version: u32,
    pub(crate) memory_bytes: u64,
    pub(crate) vcpu_count: u32,
}

impl SnapshotVmConfig {
    pub(crate) fn memory_mib(&self) -> u64 {
        self.memory_bytes / 1024 / 1024
    }

    pub(crate) fn matches(&self, cpus: u8, memory_mib: u32) -> bool {
        self.vcpu_count == cpus as u32 && self.memory_mib() == memory_mib as u64
    }
}

pub(crate) fn snapshot_generation_id(snapshot: &Path) -> String {
    read_snapshot_generation_id(snapshot).unwrap_or_else(|| legacy_snapshot_generation_id(snapshot))
}

pub(crate) fn read_snapshot_generation_id(snapshot: &Path) -> Option<String> {
    let meta = fs::read_to_string(snapshot.join(SNAPSHOT_LIFECYCLE_META)).ok()?;
    meta.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key == "generation_id" && !value.is_empty()).then(|| log_value(value))
    })
}

fn legacy_snapshot_generation_id(snapshot: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(snapshot.to_string_lossy().as_bytes());
    for name in ["vmstate.bin", "pages.img", "rootfs.ext4"] {
        hasher.update([0]);
        hasher.update(name.as_bytes());
        hasher.update([0]);
        match snapshot_file_fingerprint(&snapshot.join(name)) {
            Ok(fingerprint) => hasher.update(fingerprint.as_bytes()),
            Err(e) => hasher.update(format!("error={e}").as_bytes()),
        }
    }
    let digest = hasher.finalize();
    let prefix = u64::from_le_bytes(digest[..8].try_into().unwrap());
    format!("legacy-{prefix:016x}")
}

fn snapshot_file_fingerprint(path: &Path) -> Result<String> {
    let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let modified = meta
        .modified()
        .ok()
        .and_then(system_time_unix_nanos)
        .map(|nanos| nanos.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    #[cfg(unix)]
    {
        Ok(format!(
            "size={} modified_unix_nanos={} dev={} ino={} blocks={}",
            meta.len(),
            modified,
            meta.dev(),
            meta.ino(),
            meta.blocks()
        ))
    }
    #[cfg(not(unix))]
    {
        Ok(format!(
            "size={} modified_unix_nanos={}",
            meta.len(),
            modified
        ))
    }
}

pub(crate) fn write_snapshot_lifecycle_manifest(
    snapshot_path: &Path,
    generation_id: &str,
    source_run_id: &str,
    source_rootfs: &Path,
) -> Result<()> {
    let mut content = String::new();
    content.push_str("version=1\n");
    content.push_str(&format!("generation_id={}\n", log_value(generation_id)));
    content.push_str(&format!("source_run_id={}\n", log_value(source_run_id)));
    content.push_str(&format!("created_unix_nanos={}\n", unix_nanos()));
    content.push_str(&format!("source_rootfs={}\n", source_rootfs.display()));
    for name in ["vmstate.bin", "pages.img", "rootfs.ext4"] {
        append_snapshot_file_manifest(&mut content, snapshot_path, name);
    }
    fs::write(snapshot_path.join(SNAPSHOT_LIFECYCLE_META), content).with_context(|| {
        format!(
            "write {}",
            snapshot_path.join(SNAPSHOT_LIFECYCLE_META).display()
        )
    })
}

fn append_snapshot_file_manifest(content: &mut String, snapshot_path: &Path, name: &str) {
    let path = snapshot_path.join(name);
    match fs::metadata(&path) {
        Ok(meta) => {
            content.push_str(&format!("{name}.size={}\n", meta.len()));
            let modified = meta
                .modified()
                .ok()
                .and_then(system_time_unix_nanos)
                .map(|nanos| nanos.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            content.push_str(&format!("{name}.modified_unix_nanos={modified}\n"));
            #[cfg(unix)]
            {
                content.push_str(&format!("{name}.dev={}\n", meta.dev()));
                content.push_str(&format!("{name}.ino={}\n", meta.ino()));
                content.push_str(&format!("{name}.blocks={}\n", meta.blocks()));
            }
        }
        Err(e) => content.push_str(&format!("{name}.stat_error={e}\n")),
    }
}

pub(crate) fn snapshot_vm_config(snapshot: &Path) -> Result<Option<SnapshotVmConfig>> {
    let path = snapshot.join("vmstate.bin");
    if !path.exists() {
        return Ok(None);
    }
    let mut file = fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
    let mut header = [0u8; 40];
    file.read_exact(&mut header)
        .with_context(|| format!("read {}", path.display()))?;
    if &header[0..8] != b"LKRNSS01" {
        bail!("bad snapshot magic in {}", path.display());
    }
    let version = u32::from_le_bytes(header[8..12].try_into().unwrap());
    if version != SNAPSHOT_VMSTATE_VERSION {
        bail!(
            "unsupported snapshot version {version} in {}",
            path.display()
        );
    }
    Ok(Some(SnapshotVmConfig {
        version,
        memory_bytes: u64::from_le_bytes(header[16..24].try_into().unwrap()),
        vcpu_count: u32::from_le_bytes(header[32..36].try_into().unwrap()),
    }))
}

pub(crate) fn snapshot_initramfs_is_compatible(snapshot_path: &Path, current_stamp: &Path) -> bool {
    let Some(snapshot_key) = initramfs_stamp_key(&snapshot_path.join("initramfs.stamp")) else {
        return false;
    };
    let Some(current_key) = initramfs_stamp_key(current_stamp) else {
        return false;
    };
    snapshot_key == current_key
}

pub(crate) fn validate_snapshot_initramfs_compatibility(
    snapshot_path: &Path,
    current_stamp: &Path,
    layout: &Layout,
) -> Result<()> {
    if snapshot_initramfs_is_compatible(snapshot_path, current_stamp) {
        return Ok(());
    }
    bail!(
        "snapshot initramfs is incompatible with the current guest agent: snapshot={} current={}\n{}",
        snapshot_path.join("initramfs.stamp").display(),
        current_stamp.display(),
        snapshot_restore_recovery_guidance(layout, snapshot_path)
    )
}

pub(crate) fn snapshot_restore_recovery_guidance(layout: &Layout, snapshot_path: &Path) -> String {
    let latest = layout.snapshot_dir.join("latest");
    let is_latest = paths_refer_to_same_snapshot(snapshot_path, &latest);
    if is_latest {
        format!(
            "recovery: lnx --instance {} snapshots clear to explicitly cold-boot",
            layout.instance
        )
    } else {
        format!(
            "recovery: provide a compatible snapshot; to explicitly cold-boot, run lnx --instance {} snapshots clear and retry without --snapshot",
            layout.instance
        )
    }
}

pub(crate) fn paths_refer_to_same_snapshot(left: &Path, right: &Path) -> bool {
    left == right
        || fs::canonicalize(left)
            .ok()
            .zip(fs::canonicalize(right).ok())
            .is_some_and(|(left, right)| left == right)
}

pub(crate) fn initramfs_stamp_key(path: &Path) -> Option<String> {
    let stamp = fs::read_to_string(path).ok()?;
    for line in stamp.lines() {
        if let Some(value) = line.strip_prefix("source=") {
            return Some(format!("source={value}"));
        }
    }
    for line in stamp.lines() {
        if let Some(value) = line.strip_prefix("sha256=") {
            return Some(format!("sha256={value}"));
        }
    }
    None
}

#[derive(Debug)]
pub(crate) struct PreparedRestore {
    pub(crate) snapshot: PathBuf,
    pub(crate) rootfs: PathBuf,
    pub(crate) generation_id: String,
}

fn restore_work_snapshot(layout: &Layout) -> PathBuf {
    layout.snapshot_dir.join(RESTORE_WORK_SNAPSHOT)
}

fn restore_work_active_marker(layout: &Layout) -> PathBuf {
    layout.snapshot_dir.join(RESTORE_WORK_ACTIVE_MARKER)
}

pub(crate) fn restore_work_is_active(layout: &Layout) -> bool {
    restore_work_snapshot(layout).exists() && restore_work_active_marker(layout).exists()
}

pub(crate) fn refuse_active_restore_work(layout: &Layout) -> Result<()> {
    if !restore_work_is_active(layout) {
        return Ok(());
    }
    let marker = restore_work_active_marker(layout);
    let generation = fs::read_to_string(&marker)
        .unwrap_or_default()
        .trim()
        .to_string();
    bail!(
        "a previous restored VM did not publish a final snapshot; refusing to delete recoverable state at {} ({})\nrecovery: preserve the directory for inspection, or run lnx --instance {} snapshots clear to explicitly discard it",
        restore_work_snapshot(layout).display(),
        if generation.is_empty() {
            "generation unknown"
        } else {
            generation.as_str()
        },
        layout.instance
    )
}

pub(crate) fn mark_restore_work_active(layout: &Layout, generation_id: &str) -> Result<()> {
    fs::create_dir_all(&layout.snapshot_dir)
        .with_context(|| format!("create {}", layout.snapshot_dir.display()))?;
    let marker = restore_work_active_marker(layout);
    fs::write(&marker, format!("generation_id={generation_id}\n"))
        .with_context(|| format!("write {}", marker.display()))
}

pub(crate) fn clear_restore_work_active(layout: &Layout) -> Result<()> {
    let marker = restore_work_active_marker(layout);
    match fs::remove_file(&marker) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", marker.display())),
    }
}

pub(crate) fn finish_restore_work_after_final_snapshot(
    layout: &Layout,
    restored_from_work: bool,
    snapshot_result: Result<()>,
) -> Result<()> {
    snapshot_result?;
    if restored_from_work {
        clear_restore_work_active(layout)?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FinalSnapshotOutcome {
    pub(crate) pid: u32,
    pub(crate) pending: bool,
    pub(crate) succeeded: bool,
    pub(crate) error: Option<String>,
}

pub(crate) fn clear_final_snapshot_outcome(layout: &Layout) -> Result<()> {
    let path = layout.snapshot_dir.join(FINAL_SNAPSHOT_OUTCOME);
    match fs::remove_file(&path) {
        Ok(()) => sync_snapshot_directory(layout),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

pub(crate) fn write_final_snapshot_outcome(
    layout: &Layout,
    snapshot_result: &Result<()>,
) -> Result<()> {
    let (status, error) = match snapshot_result {
        Ok(()) => ("success", None),
        Err(error) => ("error", Some(super::log_value(&format!("{error:#}")))),
    };
    write_final_snapshot_outcome_status(layout, std::process::id(), status, error.as_deref())
}

pub(crate) fn write_final_snapshot_pending(layout: &Layout) -> Result<()> {
    write_final_snapshot_outcome_status(layout, std::process::id(), "pending", None)
}

pub(crate) fn write_final_snapshot_failure(layout: &Layout, pid: u32, error: &str) -> Result<()> {
    let error = super::log_value(error);
    write_final_snapshot_outcome_status(layout, pid, "error", Some(&error))
}

pub(crate) fn acknowledge_final_snapshot_outcome(layout: &Layout, pid: u32) -> Result<()> {
    let path = layout.snapshot_dir.join(FINAL_SNAPSHOT_OUTCOME);
    match fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(mut file) => {
            if !file
                .metadata()
                .with_context(|| format!("stat {}", path.display()))?
                .is_file()
            {
                bail!(
                    "final snapshot outcome is not a regular file: {}",
                    path.display()
                );
            }
            let content = format!("version=1\npid={pid}\nstatus=cleared\n");
            file.write_all(content.as_bytes())
                .with_context(|| format!("acknowledge {}", path.display()))?;
            file.set_len(content.len() as u64)
                .with_context(|| format!("truncate {}", path.display()))?;
            file.sync_data()
                .with_context(|| format!("sync {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            write_final_snapshot_outcome_status(layout, pid, "cleared", None)
        }
        Err(error) => Err(error).with_context(|| format!("open {}", path.display())),
    }
}

fn write_final_snapshot_outcome_status(
    layout: &Layout,
    pid: u32,
    status: &str,
    error: Option<&str>,
) -> Result<()> {
    fs::create_dir_all(&layout.snapshot_dir)
        .with_context(|| format!("create {}", layout.snapshot_dir.display()))?;
    let path = layout.snapshot_dir.join(FINAL_SNAPSHOT_OUTCOME);
    let (temp, mut file) = (0_u32..1000)
        .find_map(|attempt| {
            let temp = layout.snapshot_dir.join(format!(
                ".{FINAL_SNAPSHOT_OUTCOME}.tmp-{}-{}-{attempt}",
                std::process::id(),
                unix_nanos()
            ));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temp)
            {
                Ok(file) => Some(Ok((temp, file))),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(error) => {
                    Some(Err(error).with_context(|| format!("create {}", temp.display())))
                }
            }
        })
        .transpose()?
        .context("could not allocate a unique final snapshot outcome temp file")?;
    let mut content = format!("version=1\npid={pid}\nstatus={status}\n");
    if let Some(error) = error {
        content.push_str(&format!("error={error}\n"));
    }
    let publish_result = (|| -> Result<()> {
        file.write_all(content.as_bytes())
            .with_context(|| format!("write {}", temp.display()))?;
        file.sync_data()
            .with_context(|| format!("sync {}", temp.display()))?;
        drop(file);
        fs::rename(&temp, &path)
            .with_context(|| format!("publish {} to {}", temp.display(), path.display()))?;
        sync_snapshot_directory(layout)
    })();
    if publish_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    publish_result
}

pub(crate) fn sync_snapshot_directory(layout: &Layout) -> Result<()> {
    fs::File::open(&layout.snapshot_dir)
        .with_context(|| format!("open {} for sync", layout.snapshot_dir.display()))?
        .sync_all()
        .with_context(|| format!("sync {}", layout.snapshot_dir.display()))
}

pub(crate) fn read_final_snapshot_outcome(layout: &Layout) -> Result<Option<FinalSnapshotOutcome>> {
    let path = layout.snapshot_dir.join(FINAL_SNAPSHOT_OUTCOME);
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
    };
    if !file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?
        .is_file()
    {
        bail!(
            "final snapshot outcome is not a regular file: {}",
            path.display()
        );
    }
    let mut content = String::new();
    file.read_to_string(&mut content)
        .with_context(|| format!("read {}", path.display()))?;
    let mut pid = None;
    let mut status = None;
    let mut error = None;
    for line in content.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "pid" => pid = value.parse::<u32>().ok(),
            "status" => status = Some(value),
            "error" => error = Some(value.to_string()),
            _ => {}
        }
    }
    let pid = pid.with_context(|| format!("invalid pid in {}", path.display()))?;
    let (pending, succeeded) = match status {
        Some("pending") => (true, false),
        Some("success" | "cleared") => (false, true),
        Some("error") => (false, false),
        _ => bail!("invalid status in {}", path.display()),
    };
    Ok(Some(FinalSnapshotOutcome {
        pid,
        pending,
        succeeded,
        error,
    }))
}

pub(crate) fn validate_final_snapshot_outcome(
    layout: &Layout,
    expected_pid: Option<u32>,
) -> Result<()> {
    let outcome = read_final_snapshot_outcome(layout)?;
    let recovery = format!(
        "recovery: lnx --instance {} snapshots clear to acknowledge and explicitly cold-boot",
        layout.instance
    );
    match (expected_pid, outcome) {
        (Some(pid), None) => bail!(
            "instance {} stopped without reporting whether its final snapshot succeeded (owner pid {pid})\n{recovery}",
            layout.instance
        ),
        (Some(pid), Some(outcome)) if outcome.pid != pid => bail!(
            "instance {} reported a final snapshot outcome for pid {}, expected pid {pid}\n{recovery}",
            layout.instance,
            outcome.pid
        ),
        (_, Some(outcome)) if outcome.pending => bail!(
            "instance {} owner pid {} did not finish reporting its final snapshot\n{recovery}",
            layout.instance,
            outcome.pid
        ),
        (_, Some(outcome)) if !outcome.succeeded => bail!(
            "instance {} final snapshot failed for owner pid {}: {}\n{recovery}",
            layout.instance,
            outcome.pid,
            outcome.error.as_deref().unwrap_or("unknown snapshot error")
        ),
        _ => Ok(()),
    }
}

pub(crate) fn validate_or_record_final_snapshot_failure(
    layout: &Layout,
    expected_pid: u32,
) -> Result<()> {
    let validation = validate_final_snapshot_outcome(layout, Some(expected_pid));
    if let Err(error) = validation {
        let already_blocks = read_final_snapshot_outcome(layout)
            .ok()
            .flatten()
            .is_some_and(|outcome| {
                outcome.pid == expected_pid && (outcome.pending || !outcome.succeeded)
            });
        if !already_blocks
            && let Err(tombstone_error) = write_final_snapshot_failure(
                layout,
                expected_pid,
                &format!("owner stopped without a valid final snapshot outcome: {error:#}"),
            )
        {
            return Err(error.context(format!(
                "also failed to persist shutdown failure evidence: {tombstone_error:#}"
            )));
        }
        return Err(error);
    }
    Ok(())
}

pub(crate) fn prepare_restore_for_start(
    layout: &Layout,
    restore_snapshot: Option<&Path>,
    restore_generation: Option<&str>,
    run_log: &RunLog,
) -> Result<Option<PreparedRestore>> {
    cleanup_snapshot_runtime_state(layout, run_log)?;
    let work_snapshot = restore_work_snapshot(layout);
    let Some(snapshot) = restore_snapshot else {
        return Ok(None);
    };
    validate_restore_snapshot_rootfs(snapshot, run_log)?;
    remove_path_if_exists(&work_snapshot)?;
    clone_restore_snapshot(snapshot, &work_snapshot)?;
    let snapshot_rootfs = snapshot.join("rootfs.ext4");
    let work_rootfs = work_snapshot.join("rootfs.ext4");
    crate::init::ensure_ext4_has_no_errors(&work_rootfs, "live restore rootfs")?;
    let generation = restore_generation
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| snapshot_generation_id(snapshot));
    run_log.line(format!(
        "snapshot.restore.clone generation_id={} source={} work={} rootfs_source={} rootfs_work={}",
        generation,
        snapshot.display(),
        work_snapshot.display(),
        snapshot_rootfs.display(),
        work_rootfs.display()
    ));
    Ok(Some(PreparedRestore {
        snapshot: work_snapshot,
        rootfs: work_rootfs,
        generation_id: generation,
    }))
}

fn clone_restore_snapshot(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).with_context(|| format!("create {}", dst.display()))?;
    for name in SNAPSHOT_RESTORE_FILES {
        let src_file = src.join(name);
        if src_file.exists() {
            clone_or_copy_file(&src_file, &dst.join(name)).with_context(|| {
                format!(
                    "clone {} to {}",
                    src_file.display(),
                    dst.join(name).display()
                )
            })?;
        }
    }
    let host_share_state = src.join("host-share-state");
    if host_share_state.exists() {
        clone_tree(&host_share_state, &dst.join("host-share-state"))?;
    }
    Ok(())
}

fn clone_tree(src: &Path, dest: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(src).with_context(|| format!("stat {}", src.display()))?;
    if metadata.is_dir() {
        fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
        for entry in fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
            let entry = entry.with_context(|| format!("read {}", src.display()))?;
            clone_tree(&entry.path(), &dest.join(entry.file_name()))?;
        }
        return Ok(());
    }
    if metadata.file_type().is_symlink() {
        let link = fs::read_link(src).with_context(|| format!("readlink {}", src.display()))?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        std::os::unix::fs::symlink(&link, dest)
            .with_context(|| format!("symlink {} to {}", link.display(), dest.display()))?;
        return Ok(());
    }
    clone_or_copy_file(src, dest)
}

pub(crate) fn cleanup_snapshot_runtime_state(layout: &Layout, run_log: &RunLog) -> Result<()> {
    let work = restore_work_snapshot(layout);
    let active_marker = restore_work_active_marker(layout);
    refuse_active_restore_work(layout)?;
    if active_marker.exists() {
        run_log.line(format!(
            "snapshot.work.active_marker.remove_without_work path={}",
            active_marker.display()
        ));
        clear_restore_work_active(layout)?;
    }
    if work.exists() {
        run_log.line(format!("snapshot.work.remove path={}", work.display()));
        remove_path_if_exists(&work)?;
    }
    cleanup_snapshot_publish_paths(&layout.snapshot_dir.join("latest"), run_log)
}

pub(crate) fn snapshot_publish_temp(snapshot_path: &Path) -> Result<PathBuf> {
    sibling_dot_path(snapshot_path, "next")
}

pub(crate) fn snapshot_publish_previous(snapshot_path: &Path) -> Result<PathBuf> {
    sibling_dot_path(snapshot_path, "previous")
}

fn sibling_dot_path(path: &Path, suffix: &str) -> Result<PathBuf> {
    let parent = path.parent().context("snapshot path has no parent")?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("snapshot path has no file name")?;
    Ok(parent.join(format!(".{name}.{suffix}")))
}

pub(crate) fn cleanup_snapshot_publish_paths(snapshot_path: &Path, run_log: &RunLog) -> Result<()> {
    let temp = snapshot_publish_temp(snapshot_path)?;
    let previous = snapshot_publish_previous(snapshot_path)?;
    if previous.exists() && !snapshot_path.exists() {
        run_log.line(format!(
            "snapshot.publish.recover previous={} latest={}",
            previous.display(),
            snapshot_path.display()
        ));
        fs::rename(&previous, snapshot_path).with_context(|| {
            format!(
                "recover {} to {}",
                previous.display(),
                snapshot_path.display()
            )
        })?;
    }
    if temp.exists() {
        run_log.line(format!(
            "snapshot.publish.temp.remove path={}",
            temp.display()
        ));
        remove_path_if_exists(&temp)?;
    }
    if previous.exists() {
        run_log.line(format!(
            "snapshot.publish.previous.remove path={}",
            previous.display()
        ));
        remove_path_if_exists(&previous)?;
    }
    Ok(())
}

fn validate_restore_snapshot_rootfs(snapshot: &Path, run_log: &RunLog) -> Result<()> {
    let rootfs = snapshot.join("rootfs.ext4");
    if !rootfs.exists() {
        bail!(
            "snapshot cannot be restored because its rootfs is missing: {}",
            rootfs.display()
        );
    }
    crate::init::ensure_ext4_has_no_errors(&rootfs, "snapshot rootfs")?;
    if let Some(reason) = snapshot_rootfs_pair_incoherent(snapshot)? {
        run_log.line(format!(
            "snapshot.rootfs.incoherent path={} reason={reason}",
            snapshot.display()
        ));
        return Err(anyhow!(reason)).context(RestoreRefused);
    }
    Ok(())
}

fn snapshot_rootfs_pair_incoherent(snapshot: &Path) -> Result<Option<String>> {
    let rootfs = snapshot.join("rootfs.ext4");
    let vmstate = snapshot.join("vmstate.bin");
    let pages = snapshot.join("pages.img");
    let rootfs_modified = file_modified_time(&rootfs)?;
    let vmstate_modified = file_modified_time(&vmstate)?;
    let pages_modified = file_modified_time(&pages)?;
    let state_modified = vmstate_modified.max(pages_modified);
    if rootfs_modified
        .duration_since(state_modified)
        .is_ok_and(|delta| delta > Duration::from_secs(1))
    {
        return Ok(Some(format!(
            "snapshot rootfs was modified after memory state was captured; refusing to pair {} with stale {}/{}",
            rootfs.display(),
            vmstate.display(),
            pages.display()
        )));
    }
    Ok(None)
}

fn file_modified_time(path: &Path) -> Result<SystemTime> {
    fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .modified()
        .with_context(|| format!("read modification time for {}", path.display()))
}

pub(crate) fn promote_snapshot_rootfs(
    snapshot_path: &Path,
    canonical_rootfs: &Path,
    timings: &TimingLog,
    run_log: &RunLog,
    generation_id: Option<&str>,
    owner_run_id: Option<&str>,
) -> Result<()> {
    let snapshot_rootfs = snapshot_path.join("rootfs.ext4");
    if snapshot_rootfs == canonical_rootfs {
        return Ok(());
    }
    if !snapshot_rootfs.exists() {
        bail!(
            "snapshot rootfs is missing after snapshot capture: {}",
            snapshot_rootfs.display()
        );
    }
    crate::init::ensure_ext4_has_no_errors(&snapshot_rootfs, "snapshot rootfs")?;
    let parent = canonical_rootfs
        .parent()
        .context("canonical rootfs path has no parent")?;
    let file_name = canonical_rootfs
        .file_name()
        .and_then(|name| name.to_str())
        .context("canonical rootfs path has no file name")?;
    let temp = parent.join(format!(".{file_name}.promote"));
    timings.event("snapshot.rootfs.promote.begin");
    run_log.line(format!(
        "snapshot.rootfs.promote owner_run_id={} generation_id={} source={} dest={}",
        owner_run_id.unwrap_or("unknown"),
        generation_id.unwrap_or("unknown"),
        snapshot_rootfs.display(),
        canonical_rootfs.display()
    ));
    remove_path_if_exists(&temp)?;
    clone_or_copy_file(&snapshot_rootfs, &temp)?;
    fs::rename(&temp, canonical_rootfs).with_context(|| {
        format!(
            "rename {} to {}",
            temp.display(),
            canonical_rootfs.display()
        )
    })?;
    timings.event("snapshot.rootfs.promote.done");
    run_log.line(format!(
        "snapshot.rootfs.promote.done owner_run_id={} generation_id={} dest={}",
        owner_run_id.unwrap_or("unknown"),
        generation_id.unwrap_or("unknown"),
        canonical_rootfs.display()
    ));
    Ok(())
}

pub(crate) fn publish_snapshot_dir(
    snapshot_path: &Path,
    temp: &Path,
    run_log: &RunLog,
    owner_run_id: &str,
    generation_id: &str,
) -> Result<()> {
    let previous = snapshot_publish_previous(snapshot_path)?;
    run_log.line(format!(
        "snapshot.publish.begin owner_run_id={} generation_id={} temp={} dest={} previous={}",
        owner_run_id,
        generation_id,
        temp.display(),
        snapshot_path.display(),
        previous.display()
    ));
    remove_path_if_exists(&previous)?;
    let had_previous = snapshot_path.exists();
    if had_previous {
        fs::rename(snapshot_path, &previous).with_context(|| {
            format!(
                "move previous snapshot {} to {}",
                snapshot_path.display(),
                previous.display()
            )
        })?;
    }
    match fs::rename(temp, snapshot_path) {
        Ok(()) => {}
        Err(e) => {
            if had_previous && previous.exists() && !snapshot_path.exists() {
                let _ = fs::rename(&previous, snapshot_path);
            }
            return Err(e).with_context(|| {
                format!("publish {} to {}", temp.display(), snapshot_path.display())
            });
        }
    }
    if previous.exists() {
        remove_path_if_exists(&previous)?;
    }
    run_log.line(format!(
        "snapshot.publish.done owner_run_id={} generation_id={} dest={}",
        owner_run_id,
        generation_id,
        snapshot_path.display()
    ));
    Ok(())
}

pub(crate) fn validate_snapshot_rootfs(snapshot_path: &Path) -> Result<()> {
    crate::init::ensure_ext4_has_no_errors(&snapshot_path.join("rootfs.ext4"), "snapshot rootfs")
}

pub(crate) fn align_snapshot_rootfs_mtime_with_memory(snapshot_path: &Path) -> Result<()> {
    let rootfs = snapshot_path.join("rootfs.ext4");
    let vmstate = snapshot_path.join("vmstate.bin");
    let pages = snapshot_path.join("pages.img");
    let state_modified = file_modified_time(&vmstate)?.max(file_modified_time(&pages)?);
    set_file_modified_time(&rootfs, state_modified)
}

fn set_file_modified_time(path: &Path, time: SystemTime) -> Result<()> {
    let duration = time
        .duration_since(UNIX_EPOCH)
        .with_context(|| format!("mtime before unix epoch for {}", path.display()))?;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("encode path {}", path.display()))?;
    let times = [
        libc::timespec {
            tv_sec: duration.as_secs() as _,
            tv_nsec: duration.subsec_nanos() as _,
        },
        libc::timespec {
            tv_sec: duration.as_secs() as _,
            tv_nsec: duration.subsec_nanos() as _,
        },
    ];
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
    if rc == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error()).with_context(|| format!("set mtime {}", path.display()))
}
