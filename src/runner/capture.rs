//! Captures a running VM owner takes on request: checkpoints asked for by
//! clients and snapshot-exits asked for by the guest. They run one at a time
//! on a dedicated worker thread, so a capture never stalls the broker's
//! accept loop.

use super::*;

pub(crate) struct CheckpointRequest {
    pub(crate) path: PathBuf,
    pub(crate) reply: mpsc::Sender<Result<(), String>>,
}

pub(crate) enum CaptureJob {
    Checkpoint(CheckpointRequest),
    SnapshotExit {
        channel_id: u64,
    },
    /// Stop after every job queued before this one has run.
    Finish,
}

/// Something that can take the captures a running owner is asked for.
pub(crate) trait Captures: Send + 'static {
    fn checkpoint(&self, request: &CheckpointRequest) -> Result<()>;
    /// Captures `latest` for a guest snapshot-exit and replies to the guest.
    fn snapshot_exit(&self, channel_id: u64);
}

/// What a capture needs to know about the running VM.
pub(crate) struct Capturer {
    pub(crate) vm: Arc<VmHandle>,
    pub(crate) layout: Layout,
    /// The rootfs the VM is running on.
    pub(crate) rootfs: PathBuf,
    /// Where snapshot-exits publish (usually `memory-snapshots/latest`).
    pub(crate) snapshot_path: PathBuf,
    pub(crate) canonical_rootfs: PathBuf,
    pub(crate) promote_rootfs_after_snapshot: bool,
    pub(crate) restore_snapshot: Option<PathBuf>,
    pub(crate) restore_generation: Option<String>,
    pub(crate) initramfs_stamp: PathBuf,
    pub(crate) deterministic_clock_state: Option<DeterministicClockState>,
    pub(crate) agent_tx: mpsc::Sender<Message>,
    pub(crate) timings: Arc<TimingLog>,
    pub(crate) run_log: Arc<RunLog>,
    pub(crate) trace_log: Option<Arc<TraceLog>>,
    pub(crate) owner_run_id: String,
}

impl Captures for Capturer {
    fn checkpoint(&self, request: &CheckpointRequest) -> Result<()> {
        let generation_id = new_lifecycle_id("snapshot");
        let path = &request.path;
        self.timings.event("checkpoint.request.begin");
        self.run_log.line(format!(
            "checkpoint.request owner_run_id={} generation_id={generation_id} path={}",
            self.owner_run_id,
            path.display()
        ));
        if let Some(trace) = &self.trace_log {
            trace.event(
                "checkpoint_request",
                vec![trace_text("path", path.display().to_string())],
            );
        }
        seed_incremental_snapshot(
            path,
            self.restore_snapshot.as_deref(),
            &self.snapshot_path,
            &self.run_log,
        )?;
        ensure_deterministic_clock_state_file(
            &self.initramfs_stamp,
            self.deterministic_clock_state.as_ref(),
        )?;
        self.run_log.line(format!(
            "checkpoint.capture.begin owner_run_id={} generation_id={generation_id} path={} source_rootfs={} source_generation={}",
            self.owner_run_id,
            path.display(),
            self.rootfs.display(),
            self.restore_generation.as_deref().unwrap_or("none")
        ));
        capture_vm_state(&self.vm, path, &self.rootfs, &self.layout)?;
        validate_snapshot_rootfs(path)?;
        align_snapshot_rootfs_mtime_with_memory(path)?;
        self.run_log.line(format!(
            "checkpoint.capture.done owner_run_id={} generation_id={generation_id} path={}",
            self.owner_run_id,
            path.display()
        ));
        copy_snapshot_stamp(
            path,
            &self.initramfs_stamp,
            self.trace_log.as_deref(),
            self.deterministic_clock_state.as_ref(),
        )?;
        write_snapshot_lifecycle_manifest(path, &generation_id, &self.owner_run_id, &self.rootfs)?;
        self.run_log.line(format!(
            "checkpoint.done owner_run_id={} generation_id={generation_id} path={}",
            self.owner_run_id,
            path.display()
        ));
        if let Some(trace) = &self.trace_log {
            trace.event(
                "checkpoint_done",
                vec![trace_text("path", path.display().to_string())],
            );
        }
        log_snapshot_summary(&self.run_log, "checkpoint", path);
        Ok(())
    }

    fn snapshot_exit(&self, channel_id: u64) {
        let generation_id = new_lifecycle_id("snapshot");
        self.timings.event("snapshot_exit.request.begin");
        self.run_log.line(format!(
            "snapshot_exit.request owner_run_id={} generation_id={generation_id} channel_id={channel_id} path={}",
            self.owner_run_id,
            self.snapshot_path.display()
        ));
        let result = capture_snapshot_for_publish(
            &self.vm,
            &self.snapshot_path,
            &self.rootfs,
            &self.initramfs_stamp,
            &self.layout,
            self.trace_log.as_deref(),
            self.deterministic_clock_state.as_ref(),
            self.restore_snapshot.as_deref(),
            false,
            &self.run_log,
            &self.owner_run_id,
            &generation_id,
        )
        .and_then(|()| {
            if self.promote_rootfs_after_snapshot {
                promote_snapshot_rootfs(
                    &self.snapshot_path,
                    &self.canonical_rootfs,
                    &self.timings,
                    &self.run_log,
                    Some(&generation_id),
                    Some(&self.owner_run_id),
                )
            } else {
                Ok(())
            }
        });
        let reply = match result {
            Ok(()) => {
                self.run_log.line(format!(
                    "snapshot_exit.done owner_run_id={} generation_id={generation_id} channel_id={channel_id} path={}",
                    self.owner_run_id,
                    self.snapshot_path.display()
                ));
                if let Some(trace) = &self.trace_log {
                    trace.event(
                        "snapshot_exit_done",
                        vec![
                            trace_text("channel_id", format!("{channel_id:016x}")),
                            trace_text("path", self.snapshot_path.display().to_string()),
                        ],
                    );
                }
                log_snapshot_summary(&self.run_log, "snapshot.latest", &self.snapshot_path);
                Message::CheckpointCreated { channel_id }
            }
            Err(error) => {
                self.run_log.line(format!(
                    "snapshot_exit.error owner_run_id={} generation_id={generation_id} channel_id={channel_id} error={error:#}",
                    self.owner_run_id
                ));
                Message::Error {
                    channel_id,
                    message: format!("snapshot-exit failed: {error:#}"),
                }
            }
        };
        let _ = self.agent_tx.send(reply);
    }
}

/// Runs capture jobs one at a time in the order they were queued.
pub(crate) struct CaptureWorker {
    jobs: mpsc::Sender<CaptureJob>,
    thread: thread::JoinHandle<()>,
}

impl CaptureWorker {
    pub(crate) fn spawn(capturer: impl Captures) -> Self {
        let (jobs, queue) = mpsc::channel::<CaptureJob>();
        let thread = thread::spawn(move || {
            while let Ok(job) = queue.recv() {
                match job {
                    CaptureJob::Checkpoint(request) => {
                        let result = capturer
                            .checkpoint(&request)
                            .map_err(|error| format!("{error:#}"));
                        let _ = request.reply.send(result);
                    }
                    CaptureJob::SnapshotExit { channel_id } => capturer.snapshot_exit(channel_id),
                    CaptureJob::Finish => break,
                }
            }
        });
        Self { jobs, thread }
    }

    pub(crate) fn jobs(&self) -> mpsc::Sender<CaptureJob> {
        self.jobs.clone()
    }

    /// Waits for every job queued so far to finish, then stops the worker.
    /// The caller must already have closed the broker's stopping barrier, so
    /// no new checkpoint can be queued behind the finish marker.
    pub(crate) fn finish(self) -> Result<()> {
        let _ = self.jobs.send(CaptureJob::Finish);
        self.thread
            .join()
            .map_err(|_| anyhow!("capture worker panicked"))
    }
}
