//! The store side of one VM owner run: from taking over the instance to
//! committing (or abandoning) the run. See `crate::store` for the layout and
//! `specs/tla/README.md` for the rules this follows.

use super::*;
use crate::store::{
    self, CheckpointRef, Generation, GenerationId, Migration, Origin, Recovery, Staging, Store,
};

/// One VM run of an instance, holding the instance lock throughout.
pub(crate) struct RunSession {
    store: Store,
    lock: InstanceLock,
    run: store::Run,
    instance: String,
    /// Whether the run has served a command since its last commit. Guarded
    /// so that marking dirty and advancing `latest` cannot interleave.
    dirty: Mutex<bool>,
    /// The generation whose `pages.img` the VM's dirty tracking is relative
    /// to: the base at first, then each generation this run captures.
    last_capture: Mutex<Option<GenerationId>>,
    run_log: Arc<RunLog>,
}

impl RunSession {
    /// Takes over the instance for a new VM run: migrates an old layout,
    /// recovers from whatever the previous owner left, and prepares the run's
    /// work directory from `latest`, or from `explicit_snapshot` when given.
    pub(crate) fn begin(
        layout: &Layout,
        lock: InstanceLock,
        explicit_snapshot: Option<&Path>,
        run_log: Arc<RunLog>,
    ) -> Result<Self> {
        let store = Store::new(&layout.instance_dir);
        if let Migration::Migrated {
            latest,
            crashed_run,
            checkpoints,
        } = store::migrate_legacy_layout(layout, &lock)?
        {
            run_log.line(format!(
                "store.migrated latest={latest} crashed_run={} checkpoints={checkpoints}",
                crashed_run
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "none".to_string())
            ));
        }
        if !store.exists() {
            bail!(
                "instance {} has no saved state; create it with lnx init",
                layout.instance
            );
        }
        match store.recover(&lock)? {
            Recovery::Clean => {}
            recovery => run_log.line(format!("store.recovered {recovery:?}")),
        }
        let from = explicit_snapshot
            .map(|dir| {
                let latest = store.record()?.and_then(|record| record.latest);
                store.import_dir(&lock, dir, latest, Origin::Import)
            })
            .transpose()?;
        let run = store.begin_run(&lock, from.as_ref())?;
        run_log.line(format!(
            "store.run.begin run={} base={} memory={} dir={}",
            run.id,
            run.base
                .as_ref()
                .map(|base| base.id().to_string())
                .unwrap_or_else(|| "none".to_string()),
            run.restores_memory(),
            run.dir.display()
        ));
        let last_capture = run.base.as_ref().map(|base| base.id().clone());
        Ok(Self {
            store,
            lock,
            run,
            instance: layout.instance.clone(),
            dirty: Mutex::new(false),
            last_capture: Mutex::new(last_capture),
            run_log,
        })
    }

    pub(crate) fn run(&self) -> &store::Run {
        &self.run
    }

    pub(crate) fn base(&self) -> Option<&Generation> {
        self.run.base.as_ref()
    }

    /// Records, before the first command of the run reaches the guest, that
    /// the run now holds state a client may rely on (S3).
    pub(crate) fn mark_dirty(&self) -> Result<()> {
        let mut dirty = self
            .dirty
            .lock()
            .map_err(|_| anyhow!("run dirty flag lock poisoned"))?;
        if !*dirty {
            self.store.mark_dirty(&self.lock, &self.run.id)?;
            self.run_log
                .line(format!("store.run.dirty run={}", self.run.id));
            *dirty = true;
        }
        Ok(())
    }

    /// Starts a generation for a capture, seeded with the memory image the
    /// VM's dirty tracking is relative to so the capture can be incremental.
    pub(crate) fn stage(&self) -> Result<Staging> {
        let staging = self.store.stage(&self.lock)?;
        let base = self
            .last_capture
            .lock()
            .map_err(|_| anyhow!("last capture lock poisoned"))?
            .clone();
        if let Some(base) = base {
            let base = self.store.generation(&base)?;
            if base.manifest.has_memory() {
                for name in [store::VMSTATE, store::PAGES] {
                    clone_or_copy_file(&base.dir.join(name), &staging.dir().join(name))?;
                }
            }
        }
        Ok(staging)
    }

    pub(crate) fn publish(&self, staging: Staging, origin: Origin) -> Result<GenerationId> {
        let parent = self.base().map(|base| base.id().clone());
        let id = self.store.publish(&self.lock, staging, parent, origin)?;
        if let Ok(mut last_capture) = self.last_capture.lock() {
            *last_capture = Some(id.clone());
        }
        Ok(id)
    }

    /// The commit point of the run's final snapshot (S5).
    pub(crate) fn commit_final(&self, id: &GenerationId) -> Result<()> {
        self.store.commit_latest(&self.lock, id)?;
        self.run_log.line(format!(
            "store.run.committed run={} latest={id}",
            self.run.id
        ));
        Ok(())
    }

    /// Makes `id` the latest generation while the run continues, as after a
    /// guest-requested snapshot.
    pub(crate) fn advance(&self, id: &GenerationId) -> Result<()> {
        let mut dirty = self
            .dirty
            .lock()
            .map_err(|_| anyhow!("run dirty flag lock poisoned"))?;
        self.store.advance_latest(&self.lock, &self.run.id, id)?;
        *dirty = false;
        Ok(())
    }

    pub(crate) fn add_checkpoint(&self, checkpoint: &CheckpointRef) -> Result<()> {
        self.store.add_checkpoint(&self.lock, checkpoint)
    }

    /// Writes a fresh instance at `dest` whose latest generation is `id`.
    pub(crate) fn export(&self, id: &GenerationId, dest: &Path) -> Result<()> {
        let pin = self.store.pin(id)?;
        Store::export(&pin, dest)
    }

    /// Ends a run that will not produce a final snapshot. A run that never
    /// served a command is dropped (O4); one that did is left for `recover`,
    /// because its disk may hold writes a client was told about.
    pub(crate) fn abandon(&self) -> Result<()> {
        let dirty = self.dirty.lock().map(|dirty| *dirty).unwrap_or(true);
        if dirty {
            self.run_log.line(format!(
                "store.run.left_dirty run={} instance={} action=recover",
                self.run.id, self.instance
            ));
            return Ok(());
        }
        self.store.abandon_run(&self.lock, &self.run.id)?;
        self.run_log
            .line(format!("store.run.abandoned run={}", self.run.id));
        Ok(())
    }
}

/// Moves an instance from the pre-store layout into the store if that has
/// not happened yet. A running owner has already done it, so a busy lock
/// means there is nothing to do.
pub(crate) fn ensure_store(layout: &Layout) -> Result<()> {
    if Store::new(&layout.instance_dir).exists() || !layout.instance_dir.exists() {
        return Ok(());
    }
    with_exclusive_instance_state(layout, |lock, _| {
        store::migrate_legacy_layout(layout, lock).map(drop)
    })?;
    Ok(())
}
