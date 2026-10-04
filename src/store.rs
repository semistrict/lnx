//! The instance store: an instance's persistent state as immutable
//! *generations*, plus one committed record that names the latest one.
//!
//! This is the target design checked by `specs/tla/SnapshotCommit/
//! SnapshotCommitTarget.tla` and `specs/tla/CheckpointForkDelete/
//! CheckpointForkDeleteTarget.tla`; the rule numbers below refer to
//! `specs/tla/README.md`.
//!
//! ```text
//! <instance>/state.json             the record (O2)
//! <instance>/generations/<gen>/     immutable once renamed into place (S1)
//!     manifest.json  rootfs.ext4  [vmstate.bin pages.img stamps]  [host-share-state/]  .pin
//! <instance>/generations/.staging-<gen>/   a generation being written (S4)
//! <instance>/runs/<run>/            the live work directory of one VM run (S2)
//! <instance>/checkpoints/<id>.ref   a named reference to a generation (C1)
//! ```
//!
//! Every mutation takes `&InstanceLock`, so only the holder of the instance
//! lock can change the store. Files are committed by writing a uniquely named
//! temporary file, syncing it, renaming it over the target and syncing the
//! directory; on macOS the syncs are `F_BARRIERFSYNC`, which orders writes
//! without waiting for the drive cache, so a crash or power loss leaves either
//! the old or the new state, never a mix.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::fsutil::remove_path_if_exists;
use crate::runner::{InstanceLock, ProcessIdentity};
use crate::sparse_copy::{clone_or_copy_file, clone_or_copy_tree};

pub(crate) const STATE_FILE: &str = "state.json";
pub(crate) const GENERATIONS_DIR: &str = "generations";
pub(crate) const RUNS_DIR: &str = "runs";
pub(crate) const CHECKPOINTS_DIR: &str = "checkpoints";
pub(crate) const MANIFEST: &str = "manifest.json";
pub(crate) const ROOTFS: &str = "rootfs.ext4";
pub(crate) const HOST_SHARE_STATE: &str = "host-share-state";
pub(crate) const VMSTATE: &str = "vmstate.bin";
pub(crate) const PAGES: &str = "pages.img";
const PIN: &str = ".pin";
const STAGING_PREFIX: &str = ".staging-";
const TRASH_PREFIX: &str = ".trash-";
const RECORD_VERSION: u32 = 1;
const MANIFEST_VERSION: u32 = 1;

/// Names a generation or a run: unique, sortable by creation time, and safe
/// to use as a single path component.
fn new_id(prefix: &str) -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}{nanos:020}-{}-{sequence}", std::process::id())
}

fn validate_component(kind: &str, value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && !value.starts_with('.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !valid {
        bail!("invalid {kind} id: {value:?}");
    }
    Ok(())
}

macro_rules! store_id {
    ($name:ident, $kind:literal, $prefix:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub(crate) struct $name(String);

        impl $name {
            pub(crate) fn new() -> Self {
                Self(new_id($prefix))
            }

            pub(crate) fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = anyhow::Error;

            fn try_from(value: String) -> Result<Self> {
                validate_component($kind, &value)?;
                Ok(Self(value))
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> String {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

store_id!(GenerationId, "generation", "g");
store_id!(RunId, "run", "r");

/// Orders this file's earlier writes before any later write to the same
/// filesystem reaches stable storage.
pub(crate) fn sync_file(file: &File) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_BARRIERFSYNC) } == 0 {
            return Ok(());
        }
    }
    file.sync_all()
}

pub(crate) fn sync_dir(path: &Path) -> Result<()> {
    let dir = File::open(path).with_context(|| format!("open {}", path.display()))?;
    sync_file(&dir).with_context(|| format!("sync {}", path.display()))
}

fn unique_temp_sibling(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(format!(".tmp-{}", new_id("")));
    path.with_file_name(name)
}

/// Atomically and durably replaces `path` with `contents`.
pub(crate) fn commit_file(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().context("committed file has no parent")?;
    let temp = unique_temp_sibling(path);
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&temp)
            .with_context(|| format!("create {}", temp.display()))?;
        file.write_all(contents)
            .with_context(|| format!("write {}", temp.display()))?;
        sync_file(&file).with_context(|| format!("sync {}", temp.display()))?;
        fs::rename(&temp, path)
            .with_context(|| format!("rename {} to {}", temp.display(), path.display()))?;
        sync_dir(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn commit_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut contents = serde_json::to_vec_pretty(value).context("encode json")?;
    contents.push(b'\n');
    commit_file(path, &contents)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    serde_json::from_slice(&contents)
        .map(Some)
        .with_context(|| format!("parse {}", path.display()))
}

/// What the instance is doing, as last committed by the lock holder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub(crate) enum Phase {
    /// No VM is running; `latest` is the whole truth.
    Stopped,
    /// A VM run started from `latest` but has not served a command, so
    /// nothing anyone was told about can be lost by discarding it.
    Running { run: RunId, owner: ProcessIdentity },
    /// A VM run served at least one command: its work directory may hold
    /// writes that were acknowledged to a client.
    Dirty { run: RunId, owner: ProcessIdentity },
}

impl Phase {
    pub(crate) fn run(&self) -> Option<&RunId> {
        match self {
            Self::Stopped => None,
            Self::Running { run, .. } | Self::Dirty { run, .. } => Some(run),
        }
    }
}

/// The one committed record of an instance (O2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub(crate) version: u32,
    pub(crate) latest: Option<GenerationId>,
    #[serde(flatten)]
    pub(crate) phase: Phase,
}

/// Why a generation exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Origin {
    /// Created from an image, before any VM ran.
    Image,
    /// The final snapshot of a VM run.
    Snapshot { run: RunId },
    /// A snapshot the guest asked for while its run went on.
    SnapshotExit { run: RunId },
    /// A checkpoint taken while a VM run was live.
    Checkpoint { run: RunId },
    /// The disk of a run that crashed after serving commands.
    Salvage { run: RunId },
    /// `latest` with its memory dropped.
    DropMemory,
    /// `latest` edited while the instance was stopped.
    Edited,
    /// Moved over from the layout lnx used before the store existed.
    Migrated,
    /// Received from another host.
    Import,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub(crate) version: u32,
    pub(crate) id: GenerationId,
    pub(crate) parent: Option<GenerationId>,
    pub(crate) origin: Origin,
    pub(crate) created_unix_nanos: u64,
    /// Size of every top-level regular file except the manifest itself.
    pub(crate) files: BTreeMap<String, u64>,
}

impl Manifest {
    pub(crate) fn has_memory(&self) -> bool {
        self.files.contains_key(VMSTATE) && self.files.contains_key(PAGES)
    }
}

/// A generation that is complete and readable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Generation {
    pub(crate) dir: PathBuf,
    pub(crate) manifest: Manifest,
}

impl Generation {
    pub(crate) fn id(&self) -> &GenerationId {
        &self.manifest.id
    }

    pub(crate) fn rootfs(&self) -> PathBuf {
        self.dir.join(ROOTFS)
    }
}

/// A run that crashed after acknowledging commands to clients. Recovering it
/// is an explicit choice: keep its disk (`salvage`) or drop it (`discard`).
#[derive(Debug)]
pub(crate) struct CrashedWithUnsavedState {
    pub(crate) instance: String,
    pub(crate) run: RunId,
}

impl fmt::Display for CrashedWithUnsavedState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let instance = &self.instance;
        write!(
            f,
            "the VM of instance {instance} stopped unexpectedly after running commands, before saving its state (run {})\n\
             recovery: `lnx --instance {instance} recover --keep` keeps its disk (losing only its memory), \
             or `lnx --instance {instance} recover --discard` returns to the last saved state",
            self.run
        )
    }
}

impl std::error::Error for CrashedWithUnsavedState {}

/// What recovery did before the instance could be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Recovery {
    /// Nothing to do.
    Clean,
    /// A crashed run had already written its final generation; it is now
    /// `latest`.
    RolledForward(GenerationId),
    /// A crashed run had not served any command; it was dropped.
    DiscardedIdleRun(RunId),
}

/// A generation being written.
#[derive(Debug)]
pub(crate) struct Staging {
    id: GenerationId,
    dir: PathBuf,
}

impl Staging {
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }
}

/// The work directory of one VM run.
#[derive(Debug, Clone)]
pub(crate) struct Run {
    pub(crate) id: RunId,
    pub(crate) dir: PathBuf,
    /// The generation the run started from.
    pub(crate) base: Option<Generation>,
}

impl Run {
    pub(crate) fn rootfs(&self) -> PathBuf {
        self.dir.join(ROOTFS)
    }

    pub(crate) fn host_share_state(&self) -> PathBuf {
        self.dir.join(HOST_SHARE_STATE)
    }

    /// Whether the run resumes from a memory snapshot rather than booting.
    pub(crate) fn restores_memory(&self) -> bool {
        self.base
            .as_ref()
            .is_some_and(|base| base.manifest.has_memory())
    }
}

/// A named reference to a generation (C1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CheckpointRef {
    pub(crate) id: String,
    pub(crate) name: Option<String>,
    pub(crate) generation: GenerationId,
    pub(crate) created_unix: u64,
}

/// Keeps a generation from being garbage collected while it is read (C4).
#[derive(Debug)]
pub(crate) struct Pin {
    _file: File,
    pub(crate) generation: Generation,
}

/// What garbage collection removed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Collected {
    pub(crate) generations: Vec<GenerationId>,
    pub(crate) runs: Vec<String>,
    pub(crate) staging: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct Store {
    dir: PathBuf,
}

impl Store {
    pub(crate) fn new(instance_dir: &Path) -> Self {
        Self {
            dir: instance_dir.to_path_buf(),
        }
    }

    /// The instance directory the store lives in.
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join(STATE_FILE)
    }

    pub(crate) fn generations_dir(&self) -> PathBuf {
        self.dir.join(GENERATIONS_DIR)
    }

    fn runs_dir(&self) -> PathBuf {
        self.dir.join(RUNS_DIR)
    }

    fn checkpoints_dir(&self) -> PathBuf {
        self.dir.join(CHECKPOINTS_DIR)
    }

    pub(crate) fn generation_dir(&self, id: &GenerationId) -> PathBuf {
        self.generations_dir().join(id.as_str())
    }

    fn run_dir(&self, id: &RunId) -> PathBuf {
        self.runs_dir().join(id.as_str())
    }

    fn instance_name(&self) -> String {
        self.dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// The error for a run that crashed after serving commands, if any.
    pub(crate) fn crashed(&self) -> Result<Option<CrashedWithUnsavedState>> {
        Ok(self.crashed_run()?.map(|run| CrashedWithUnsavedState {
            instance: self.instance_name(),
            run,
        }))
    }

    /// Whether this instance keeps its state in the store.
    pub(crate) fn exists(&self) -> bool {
        self.state_path().exists()
    }

    pub(crate) fn record(&self) -> Result<Option<Record>> {
        let record: Option<Record> = read_json(&self.state_path())?;
        if let Some(record) = &record
            && record.version != RECORD_VERSION
        {
            bail!(
                "unsupported instance state version {} in {}",
                record.version,
                self.state_path().display()
            );
        }
        Ok(record)
    }

    fn require_record(&self) -> Result<Record> {
        self.record()?.with_context(|| {
            format!(
                "instance has no committed state: {}",
                self.state_path().display()
            )
        })
    }

    fn commit_record(
        &self,
        _lock: &InstanceLock,
        latest: Option<GenerationId>,
        phase: Phase,
    ) -> Result<()> {
        commit_json(
            &self.state_path(),
            &Record {
                version: RECORD_VERSION,
                latest,
                phase,
            },
        )
    }

    /// Reads a generation, checking that every file its manifest lists is
    /// present with the recorded size.
    pub(crate) fn generation(&self, id: &GenerationId) -> Result<Generation> {
        let dir = self.generation_dir(id);
        let manifest: Manifest = read_json(&dir.join(MANIFEST))?
            .with_context(|| format!("generation {id} has no manifest in {}", dir.display()))?;
        if &manifest.id != id {
            bail!(
                "generation {} has the manifest of generation {}",
                id,
                manifest.id
            );
        }
        for (name, size) in &manifest.files {
            let path = dir.join(name);
            let actual = fs::symlink_metadata(&path)
                .with_context(|| format!("generation {id} is missing {}", path.display()))?;
            if !actual.is_file() || actual.len() != *size {
                bail!(
                    "generation {id} file {} has size {} instead of {size}",
                    path.display(),
                    actual.len()
                );
            }
        }
        if !manifest.files.contains_key(ROOTFS) {
            bail!("generation {id} has no {ROOTFS}");
        }
        Ok(Generation { dir, manifest })
    }

    /// The latest generation, if the instance has one.
    pub(crate) fn latest(&self) -> Result<Option<Generation>> {
        match self.record()?.and_then(|record| record.latest) {
            Some(id) => self.generation(&id).map(Some),
            None => Ok(None),
        }
    }

    /// Creates an empty store whose first generation will come from
    /// `staging` (an instance created from an image).
    pub(crate) fn initialize(&self, lock: &InstanceLock, staging: Staging) -> Result<GenerationId> {
        if self.exists() {
            bail!(
                "instance state already exists: {}",
                self.state_path().display()
            );
        }
        let id = self.publish(lock, staging, None, Origin::Image)?;
        self.commit_record(lock, Some(id.clone()), Phase::Stopped)?;
        Ok(id)
    }

    /// Makes the store usable after whatever happened to the previous holder
    /// of the instance lock (O3). Never deletes `latest`.
    pub(crate) fn recover(&self, lock: &InstanceLock) -> Result<Recovery> {
        let record = self.require_record()?;
        let recovery = match &record.phase {
            Phase::Stopped => Recovery::Clean,
            Phase::Running { run, .. } | Phase::Dirty { run, .. } => {
                if let Some(id) = self.final_generation_of(run)? {
                    self.commit_record(lock, Some(id.clone()), Phase::Stopped)?;
                    Recovery::RolledForward(id)
                } else if matches!(record.phase, Phase::Running { .. }) {
                    self.commit_record(lock, record.latest.clone(), Phase::Stopped)?;
                    Recovery::DiscardedIdleRun(run.clone())
                } else {
                    return Err(CrashedWithUnsavedState {
                        instance: self.instance_name(),
                        run: run.clone(),
                    }
                    .into());
                }
            }
        };
        self.collect_garbage(lock)?;
        Ok(recovery)
    }

    /// The published final snapshot of `run`, if it got that far.
    fn final_generation_of(&self, run: &RunId) -> Result<Option<GenerationId>> {
        let mut found = None;
        for id in self.generation_ids()? {
            let Ok(generation) = self.generation(&id) else {
                continue;
            };
            if matches!(&generation.manifest.origin, Origin::Snapshot { run: origin } if origin == run)
            {
                found = Some(id);
            }
        }
        Ok(found)
    }

    fn generation_ids(&self) -> Result<Vec<GenerationId>> {
        let entries = match fs::read_dir(self.generations_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read {}", self.generations_dir().display()));
            }
        };
        let mut ids = Vec::new();
        for entry in entries {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if let Ok(id) = GenerationId::try_from(name) {
                ids.push(id);
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// Prepares a run from `latest` (or from `from`, when given) and records
    /// it as running (S2). The run directory holds private clones, so the VM
    /// never writes a generation.
    pub(crate) fn begin_run(
        &self,
        lock: &InstanceLock,
        from: Option<&GenerationId>,
    ) -> Result<Run> {
        let record = self.require_record()?;
        if record.phase != Phase::Stopped {
            bail!("instance is not stopped; recover it before starting a run");
        }
        let base = match from.or(record.latest.as_ref()) {
            Some(id) => Some(self.generation(id)?),
            None => None,
        };
        let id = RunId::new();
        let dir = self.run_dir(&id);
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        if let Some(base) = &base {
            for name in base.manifest.files.keys() {
                clone_or_copy_file(&base.dir.join(name), &dir.join(name))?;
            }
            let shares = base.dir.join(HOST_SHARE_STATE);
            if shares.exists() {
                clone_or_copy_tree(&shares, &dir.join(HOST_SHARE_STATE))?;
            }
        }
        sync_tree(&dir)?;
        sync_dir(&self.runs_dir())?;
        self.commit_record(
            lock,
            record.latest.clone(),
            Phase::Running {
                run: id.clone(),
                owner: ProcessIdentity::current(),
            },
        )?;
        Ok(Run { id, dir, base })
    }

    /// Makes `id` the latest generation while `run` keeps running, as after
    /// a guest-requested snapshot. A run that has served commands stays
    /// dirty: commands may still be running (the one that asked for the
    /// snapshot, at least), so writes acknowledged after `id` may exist, and
    /// a crash from here must still be resolved with `recover`.
    pub(crate) fn advance_latest(
        &self,
        lock: &InstanceLock,
        run: &RunId,
        id: &GenerationId,
    ) -> Result<()> {
        self.generation(id)?;
        let record = self.require_record()?;
        let phase = match &record.phase {
            Phase::Running { run: current, .. } | Phase::Dirty { run: current, .. }
                if current == run =>
            {
                record.phase.clone()
            }
            phase => bail!("cannot advance run {run}: instance is {phase:?}"),
        };
        self.commit_record(lock, Some(id.clone()), phase)
    }

    /// Copies a snapshot directory produced elsewhere (another host, an
    /// explicit `--snapshot`) into a new generation.
    pub(crate) fn import_dir(
        &self,
        lock: &InstanceLock,
        source: &Path,
        parent: Option<GenerationId>,
        origin: Origin,
    ) -> Result<GenerationId> {
        let staging = self.stage(lock)?;
        for entry in fs::read_dir(source).with_context(|| format!("read {}", source.display()))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let file_type = entry.file_type()?;
            if file_type.is_file() && name != MANIFEST && name != PIN {
                clone_or_copy_file(&entry.path(), &staging.dir.join(&name))?;
            } else if file_type.is_dir() && name == HOST_SHARE_STATE {
                clone_or_copy_tree(&entry.path(), &staging.dir.join(&name))?;
            }
        }
        self.publish(lock, staging, parent, origin)
    }

    /// Creates a new instance's store at `dest` whose latest generation is a
    /// copy of `pin`. `dest` must not exist yet.
    pub(crate) fn export(pin: &Pin, dest: &Path) -> Result<()> {
        let target = Store::new(dest);
        let generation = &pin.generation;
        let dest_dir = target.generation_dir(generation.id());
        fs::create_dir_all(&dest_dir).with_context(|| format!("create {}", dest_dir.display()))?;
        for name in generation
            .manifest
            .files
            .keys()
            .map(String::as_str)
            .chain([MANIFEST, PIN])
        {
            clone_or_copy_file(&generation.dir.join(name), &dest_dir.join(name))?;
        }
        let shares = generation.dir.join(HOST_SHARE_STATE);
        if shares.exists() {
            clone_or_copy_tree(&shares, &dest_dir.join(HOST_SHARE_STATE))?;
        }
        sync_tree(&dest_dir)?;
        sync_dir(&target.generations_dir())?;
        commit_json(
            &target.state_path(),
            &Record {
                version: RECORD_VERSION,
                latest: Some(generation.id().clone()),
                phase: Phase::Stopped,
            },
        )
    }

    /// Records that `run` is about to serve its first command (S3). From
    /// here on, a crash leaves state that only the user may discard.
    pub(crate) fn mark_dirty(&self, lock: &InstanceLock, run: &RunId) -> Result<()> {
        let record = self.require_record()?;
        match &record.phase {
            Phase::Running {
                run: current,
                owner,
            } if current == run => self.commit_record(
                lock,
                record.latest.clone(),
                Phase::Dirty {
                    run: run.clone(),
                    owner: *owner,
                },
            ),
            Phase::Dirty { run: current, .. } if current == run => Ok(()),
            phase => bail!("cannot mark run {run} dirty: instance is {phase:?}"),
        }
    }

    /// Ends a run without a new generation, keeping `latest` (O4). Only for
    /// runs that never served a command.
    pub(crate) fn abandon_run(&self, lock: &InstanceLock, run: &RunId) -> Result<()> {
        let record = self.require_record()?;
        match &record.phase {
            Phase::Running { run: current, .. } if current == run => {}
            phase => bail!("cannot abandon run {run}: instance is {phase:?}"),
        }
        self.commit_record(lock, record.latest.clone(), Phase::Stopped)?;
        self.collect_garbage(lock).map(drop)
    }

    /// Starts writing a new generation.
    pub(crate) fn stage(&self, _lock: &InstanceLock) -> Result<Staging> {
        let id = GenerationId::new();
        let dir = self
            .generations_dir()
            .join(format!("{STAGING_PREFIX}{}", id.as_str()));
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        Ok(Staging { id, dir })
    }

    /// Makes a staged generation durable and immutable (S4). It is not
    /// `latest` until committed.
    pub(crate) fn publish(
        &self,
        _lock: &InstanceLock,
        staging: Staging,
        parent: Option<GenerationId>,
        origin: Origin,
    ) -> Result<GenerationId> {
        let mut files = BTreeMap::new();
        for entry in
            fs::read_dir(&staging.dir).with_context(|| format!("read {}", staging.dir.display()))?
        {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let metadata = entry.metadata()?;
            if metadata.is_file() && name != MANIFEST && name != PIN {
                files.insert(name, metadata.len());
            }
        }
        if !files.contains_key(ROOTFS) {
            bail!("staged generation {} has no {ROOTFS}", staging.id);
        }
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            id: staging.id.clone(),
            parent,
            origin,
            created_unix_nanos: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos() as u64)
                .unwrap_or_default(),
            files,
        };
        let mut manifest_bytes = serde_json::to_vec_pretty(&manifest).context("encode manifest")?;
        manifest_bytes.push(b'\n');
        fs::write(staging.dir.join(MANIFEST), manifest_bytes)
            .with_context(|| format!("write manifest in {}", staging.dir.display()))?;
        File::create(staging.dir.join(PIN))
            .with_context(|| format!("create pin in {}", staging.dir.display()))?;
        sync_tree(&staging.dir)?;
        let dest = self.generation_dir(&staging.id);
        fs::rename(&staging.dir, &dest)
            .with_context(|| format!("publish {} to {}", staging.dir.display(), dest.display()))?;
        sync_dir(&self.generations_dir())?;
        Ok(staging.id)
    }

    /// The commit point of a snapshot (S5): `id` becomes `latest` and the
    /// instance is stopped. Garbage is collected only after (S6).
    pub(crate) fn commit_latest(&self, lock: &InstanceLock, id: &GenerationId) -> Result<()> {
        self.generation(id)?;
        self.commit_record(lock, Some(id.clone()), Phase::Stopped)?;
        self.collect_garbage(lock).map(drop)
    }

    /// Keeps the disk of a crashed dirty run as the new `latest` (without
    /// memory), so every write a client was told about survives.
    pub(crate) fn salvage(&self, lock: &InstanceLock) -> Result<GenerationId> {
        let record = self.require_record()?;
        let Phase::Dirty { run, .. } = &record.phase else {
            bail!("there is no crashed run to salvage");
        };
        let run_dir = self.run_dir(run);
        let staging = self.stage(lock)?;
        clone_or_copy_file(&run_dir.join(ROOTFS), &staging.dir.join(ROOTFS))?;
        let shares = run_dir.join(HOST_SHARE_STATE);
        if shares.exists() {
            clone_or_copy_tree(&shares, &staging.dir.join(HOST_SHARE_STATE))?;
        }
        let id = self.publish(
            lock,
            staging,
            record.latest.clone(),
            Origin::Salvage { run: run.clone() },
        )?;
        self.commit_latest(lock, &id)?;
        Ok(id)
    }

    /// Drops a crashed dirty run, returning to `latest`.
    pub(crate) fn discard_run(&self, lock: &InstanceLock) -> Result<RunId> {
        let record = self.require_record()?;
        let Phase::Dirty { run, .. } = &record.phase else {
            bail!("there is no crashed run to discard");
        };
        let run = run.clone();
        self.commit_record(lock, record.latest.clone(), Phase::Stopped)?;
        self.collect_garbage(lock)?;
        Ok(run)
    }

    /// Replaces `latest` with an edited copy: `edit` gets the staging
    /// directory holding clones of every file of `latest`.
    pub(crate) fn derive(
        &self,
        lock: &InstanceLock,
        edit: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<GenerationId> {
        let record = self.require_record()?;
        if record.phase != Phase::Stopped {
            bail!("the instance must be stopped to edit its saved state");
        }
        let latest = record
            .latest
            .as_ref()
            .map(|id| self.generation(id))
            .transpose()?
            .context("instance has no saved state to edit")?;
        let staging = self.stage(lock)?;
        for name in latest.manifest.files.keys() {
            clone_or_copy_file(&latest.dir.join(name), &staging.dir.join(name))?;
        }
        let shares = latest.dir.join(HOST_SHARE_STATE);
        if shares.exists() {
            clone_or_copy_tree(&shares, &staging.dir.join(HOST_SHARE_STATE))?;
        }
        edit(&staging.dir)?;
        let id = self.publish(lock, staging, Some(latest.id().clone()), Origin::Edited)?;
        self.commit_latest(lock, &id)?;
        Ok(id)
    }

    /// Where the instance's host-share copy-on-write state lives right now:
    /// in the running VM's work directory, or else in `latest`.
    pub(crate) fn current_host_share_state(&self) -> Result<Option<PathBuf>> {
        let Some(record) = self.record()? else {
            return Ok(None);
        };
        Ok(match (record.phase.run(), &record.latest) {
            (Some(run), _) => Some(self.run_dir(run).join(HOST_SHARE_STATE)),
            (None, Some(latest)) => Some(self.generation_dir(latest).join(HOST_SHARE_STATE)),
            (None, None) => None,
        })
    }

    /// Replaces `latest` with a copy that has no memory (S7): the next run
    /// boots from the same disk instead of resuming.
    pub(crate) fn drop_memory(&self, lock: &InstanceLock) -> Result<Option<GenerationId>> {
        let record = self.require_record()?;
        if record.phase != Phase::Stopped {
            bail!("the instance must be stopped to drop its memory snapshot");
        }
        let Some(latest) = record
            .latest
            .as_ref()
            .map(|id| self.generation(id))
            .transpose()?
        else {
            return Ok(None);
        };
        if !latest.manifest.has_memory() {
            return Ok(None);
        }
        let staging = self.stage(lock)?;
        clone_or_copy_file(&latest.rootfs(), &staging.dir.join(ROOTFS))?;
        let shares = latest.dir.join(HOST_SHARE_STATE);
        if shares.exists() {
            clone_or_copy_tree(&shares, &staging.dir.join(HOST_SHARE_STATE))?;
        }
        let id = self.publish(lock, staging, Some(latest.id().clone()), Origin::DropMemory)?;
        self.commit_latest(lock, &id)?;
        Ok(Some(id))
    }

    fn checkpoint_ref_path(&self, id: &str) -> Result<PathBuf> {
        validate_component("checkpoint", id)?;
        Ok(self.checkpoints_dir().join(format!("{id}.ref")))
    }

    /// Names a published generation as a checkpoint (C1).
    pub(crate) fn add_checkpoint(
        &self,
        _lock: &InstanceLock,
        checkpoint: &CheckpointRef,
    ) -> Result<()> {
        self.generation(&checkpoint.generation)?;
        fs::create_dir_all(self.checkpoints_dir())
            .with_context(|| format!("create {}", self.checkpoints_dir().display()))?;
        commit_json(&self.checkpoint_ref_path(&checkpoint.id)?, checkpoint)
    }

    /// Checkpoints whose generation exists, oldest first (C2).
    pub(crate) fn checkpoints(&self) -> Result<Vec<CheckpointRef>> {
        let entries = match fs::read_dir(self.checkpoints_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read {}", self.checkpoints_dir().display()));
            }
        };
        let mut checkpoints = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("ref") {
                continue;
            }
            let Some(checkpoint) = read_json::<CheckpointRef>(&path)? else {
                continue;
            };
            if self.generation_dir(&checkpoint.generation).is_dir() {
                checkpoints.push(checkpoint);
            }
        }
        checkpoints.sort_by(|a, b| (a.created_unix, &a.id).cmp(&(b.created_unix, &b.id)));
        Ok(checkpoints)
    }

    /// Unlinks a checkpoint reference (C3); its generation is reclaimed by
    /// garbage collection once nothing else names it.
    pub(crate) fn remove_checkpoint(&self, lock: &InstanceLock, id: &str) -> Result<()> {
        let path = self.checkpoint_ref_path(id)?;
        fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        sync_dir(&self.checkpoints_dir())?;
        self.collect_garbage(lock).map(drop)
    }

    /// Holds a generation in place while it is copied elsewhere (C4).
    pub(crate) fn pin(&self, id: &GenerationId) -> Result<Pin> {
        let dir = self.generation_dir(id);
        let file = File::open(dir.join(PIN))
            .with_context(|| format!("generation {id} no longer exists"))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("pin generation {id}"));
        }
        // Garbage collection renames a generation away only after locking
        // its pin exclusively, so a generation still in place now stays.
        let generation = self
            .generation(id)
            .with_context(|| format!("generation {id} no longer exists"))?;
        Ok(Pin {
            _file: file,
            generation,
        })
    }

    /// Removes runs, staging directories and every generation that neither
    /// the record nor a checkpoint names and that nobody has pinned (S6, C5).
    pub(crate) fn collect_garbage(&self, _lock: &InstanceLock) -> Result<Collected> {
        let record = self.require_record()?;
        let mut referenced: BTreeSet<GenerationId> = record.latest.iter().cloned().collect();
        referenced.extend(
            self.checkpoints()?
                .into_iter()
                .map(|checkpoint| checkpoint.generation),
        );
        let live_run = record.phase.run().cloned();
        let mut collected = Collected::default();

        if let Ok(entries) = fs::read_dir(self.runs_dir()) {
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if live_run.as_ref().is_some_and(|run| run.as_str() == name) {
                    continue;
                }
                remove_path_if_exists(&entry.path())?;
                collected.runs.push(name);
            }
        }

        if let Ok(entries) = fs::read_dir(self.generations_dir()) {
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(STAGING_PREFIX) || name.starts_with(TRASH_PREFIX) {
                    remove_path_if_exists(&entry.path())?;
                    collected.staging.push(name);
                    continue;
                }
                let Ok(id) = GenerationId::try_from(name) else {
                    continue;
                };
                if referenced.contains(&id) {
                    continue;
                }
                let pin = match File::open(entry.path().join(PIN)) {
                    Ok(pin) => Some(pin),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => return Err(error).context("open generation pin"),
                };
                if let Some(pin) = &pin
                    && unsafe { libc::flock(pin.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
                {
                    continue;
                }
                let trash = self
                    .generations_dir()
                    .join(format!("{TRASH_PREFIX}{}", id.as_str()));
                fs::rename(entry.path(), &trash)
                    .with_context(|| format!("move {} to trash", entry.path().display()))?;
                drop(pin);
                remove_path_if_exists(&trash)?;
                collected.generations.push(id);
            }
        }
        Ok(collected)
    }

    /// The run a crashed owner left behind, with the generation it started
    /// from, for diagnostics and recovery commands.
    pub(crate) fn crashed_run(&self) -> Result<Option<RunId>> {
        Ok(match self.record()?.map(|record| record.phase) {
            Some(Phase::Dirty { run, .. }) => Some(run),
            _ => None,
        })
    }
}

/// Syncs every regular file under `dir`, then each directory bottom-up.
pub(crate) fn sync_tree(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            sync_tree(&entry.path())?;
        } else if file_type.is_file() {
            let file = File::open(entry.path())
                .with_context(|| format!("open {}", entry.path().display()))?;
            sync_file(&file).with_context(|| format!("sync {}", entry.path().display()))?;
        }
    }
    sync_dir(dir)
}

mod legacy;
pub(crate) use legacy::{Migration, migrate_legacy_layout};

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::paths::Layout;
    use crate::runner::test_support::hold_as_owner;

    /// Creates `layout`'s instance with a disk holding `disk` and, when
    /// `memory` is set, a memory snapshot.
    pub(crate) fn initialized(layout: &Layout, disk: &[u8], memory: bool) -> GenerationId {
        let lock = hold_as_owner(layout);
        let store = Store::new(&layout.instance_dir);
        let staging = store.stage(&lock).expect("stage generation");
        fs::write(staging.dir().join(ROOTFS), disk).expect("write disk");
        if memory {
            // A vmstate header for 2 vCPUs and 4 GiB, as a capture writes.
            let mut header = [0u8; 40];
            header[0..8].copy_from_slice(b"LKRNSS01");
            header[8..12].copy_from_slice(&crate::runner::SNAPSHOT_VMSTATE_VERSION.to_le_bytes());
            header[16..24].copy_from_slice(&(4096u64 * 1024 * 1024).to_le_bytes());
            header[32..36].copy_from_slice(&2u32.to_le_bytes());
            fs::write(staging.dir().join(VMSTATE), header).expect("write vmstate");
            fs::write(staging.dir().join(PAGES), b"pages").expect("write pages");
        }
        store.initialize(&lock, staging).expect("initialize store")
    }

    /// Leaves `layout`'s instance as an owner that crashed after serving a
    /// command would, with `disk` as the crashed run's disk.
    pub(crate) fn crash_after_serving(layout: &Layout, disk: &[u8]) -> RunId {
        let lock = hold_as_owner(layout);
        let store = Store::new(&layout.instance_dir);
        let run = store.begin_run(&lock, None).expect("begin run");
        store.mark_dirty(&lock, &run.id).expect("mark dirty");
        fs::write(run.rootfs(), disk).expect("guest writes");
        run.id
    }

    /// Leaves `layout`'s instance as an owner that died before serving a
    /// command would.
    pub(crate) fn crash_before_serving(layout: &Layout) -> RunId {
        let lock = hold_as_owner(layout);
        Store::new(&layout.instance_dir)
            .begin_run(&lock, None)
            .expect("begin run")
            .id
    }

    pub(crate) fn latest_disk(layout: &Layout) -> Vec<u8> {
        let latest = Store::new(&layout.instance_dir)
            .latest()
            .expect("read latest")
            .expect("latest");
        fs::read(latest.rootfs()).expect("read latest disk")
    }
}

#[cfg(test)]
mod tests;
