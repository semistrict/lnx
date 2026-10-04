//! Captures a running VM owner takes: its final snapshot, checkpoints asked
//! for by clients, and snapshot-exits asked for by the guest. Requested
//! captures run one at a time on a dedicated worker thread, so a capture never
//! stalls the broker's accept loop.

use serde::{Deserialize, Serialize};

use super::*;
use crate::store::CheckpointRef;

/// What a client asks for in a checkpoint request. It travels JSON-encoded in
/// the `request` field of the protocol's `Checkpoint` message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CheckpointSpec {
    /// Record the capture as a named checkpoint of the instance.
    pub(crate) id: Option<String>,
    pub(crate) name: Option<String>,
    /// Also write a new instance at this path whose latest generation is the
    /// capture (fork and push of a running instance).
    pub(crate) export_to: Option<PathBuf>,
}

pub(crate) struct CheckpointRequest {
    pub(crate) spec: CheckpointSpec,
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

/// What capturing the running VM into a generation needs.
pub(crate) struct CaptureContext<'a> {
    pub(crate) vm: &'a VmHandle,
    pub(crate) session: &'a RunSession,
    pub(crate) initramfs_stamp: &'a Path,
    pub(crate) trace_log: Option<&'a TraceLog>,
    pub(crate) deterministic_clock_state: Option<&'a DeterministicClockState>,
}

impl CaptureContext<'_> {
    /// Captures memory, disk and host-share state at one paused instant into
    /// a new, published (not yet latest) generation.
    pub(crate) fn capture(&self, origin: Origin) -> Result<GenerationId> {
        let staging = self.session.stage()?;
        ensure_deterministic_clock_state_file(
            self.initramfs_stamp,
            self.deterministic_clock_state,
        )?;
        capture_vm_state(self.vm, staging.dir(), self.session.run())?;
        validate_snapshot_rootfs(staging.dir())?;
        copy_snapshot_stamp(
            staging.dir(),
            self.initramfs_stamp,
            self.trace_log,
            self.deterministic_clock_state,
        )?;
        self.session.publish(staging, origin)
    }
}

/// Something that can take the captures a running owner is asked for.
pub(crate) trait Captures: Send + 'static {
    fn checkpoint(&self, spec: &CheckpointSpec) -> Result<()>;
    /// Captures a new latest generation for a guest snapshot-exit and replies
    /// to the guest.
    fn snapshot_exit(&self, channel_id: u64);
}

/// The running VM's side of [`Captures`].
pub(crate) struct Capturer {
    pub(crate) vm: Arc<VmHandle>,
    pub(crate) session: Arc<RunSession>,
    pub(crate) initramfs_stamp: PathBuf,
    pub(crate) deterministic_clock_state: Option<DeterministicClockState>,
    pub(crate) agent_tx: mpsc::Sender<Message>,
    pub(crate) timings: Arc<TimingLog>,
    pub(crate) run_log: Arc<RunLog>,
    pub(crate) trace_log: Option<Arc<TraceLog>>,
    pub(crate) owner_run_id: String,
}

impl Capturer {
    fn context(&self) -> CaptureContext<'_> {
        CaptureContext {
            vm: &self.vm,
            session: &self.session,
            initramfs_stamp: &self.initramfs_stamp,
            trace_log: self.trace_log.as_deref(),
            deterministic_clock_state: self.deterministic_clock_state.as_ref(),
        }
    }

    fn run_id(&self) -> store::RunId {
        self.session.run().id.clone()
    }
}

impl Captures for Capturer {
    fn checkpoint(&self, spec: &CheckpointSpec) -> Result<()> {
        self.timings.event("checkpoint.request.begin");
        self.run_log.line(format!(
            "checkpoint.request owner_run_id={} id={} export_to={}",
            self.owner_run_id,
            spec.id.as_deref().unwrap_or("none"),
            spec.export_to
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "none".to_string())
        ));
        if let Some(trace) = &self.trace_log {
            trace.event("checkpoint_request", Vec::new());
        }
        let generation = self
            .context()
            .capture(Origin::Checkpoint { run: self.run_id() })?;
        if let Some(id) = &spec.id {
            self.session.add_checkpoint(&CheckpointRef {
                id: id.clone(),
                name: spec.name.clone(),
                generation: generation.clone(),
                created_unix: unix_seconds(),
            })?;
        }
        if let Some(dest) = &spec.export_to {
            self.session.export(&generation, dest)?;
        }
        self.run_log.line(format!(
            "checkpoint.done owner_run_id={} generation={generation}",
            self.owner_run_id
        ));
        if let Some(trace) = &self.trace_log {
            trace.event("checkpoint_done", Vec::new());
        }
        Ok(())
    }

    fn snapshot_exit(&self, channel_id: u64) {
        self.timings.event("snapshot_exit.request.begin");
        self.run_log.line(format!(
            "snapshot_exit.request owner_run_id={} channel_id={channel_id}",
            self.owner_run_id
        ));
        let result = self
            .context()
            .capture(Origin::SnapshotExit { run: self.run_id() })
            .and_then(|id| {
                self.session.advance(&id)?;
                Ok(id)
            });
        let reply = match result {
            Ok(id) => {
                self.run_log.line(format!(
                    "snapshot_exit.done owner_run_id={} channel_id={channel_id} generation={id}",
                    self.owner_run_id
                ));
                if let Some(trace) = &self.trace_log {
                    trace.event(
                        "snapshot_exit_done",
                        vec![trace_text("channel_id", format!("{channel_id:016x}"))],
                    );
                }
                Message::CheckpointCreated { channel_id }
            }
            Err(error) => {
                self.run_log.line(format!(
                    "snapshot_exit.error owner_run_id={} channel_id={channel_id} error={error:#}",
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

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
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
                            .checkpoint(&request.spec)
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
