# Reliability audit (2026-10-03)

A read-only audit of the host-side runtime on `main` @ 3bbcb655, plus what was
observed running it on macOS 27.0.1. Line numbers refer to that commit.

## Where the unreliability comes from

Instance lifecycle state is not modelled anywhere. It is re-derived on every
command by probing about a dozen filesystem artifacts (`bootstrap.lock.d/`,
`owner-start.lock.d/`, `broker.sock`, `memory-snapshots/{latest,.latest.next,
.latest.previous,.restore-work,.restore-work.active,final-snapshot.outcome}`,
`launch.json`, stamps) plus process liveness, from several places (`cli.rs`,
`server.rs`, `runner.rs`). Fixes over time added more validation layers and
"refuse; run `snapshots clear`" errors rather than fixing the ownership model,
which turns races into hard errors and makes the only recovery path destructive.

Bug history (92 commits touching `src/`): the dominant cluster is snapshot /
restore lifecycle and recovery, then vsock reconnect after restore, then
owner/lock startup races.

## Confirmed hazards

1. Locks are pid-file directories. They are not released on crash, a recycled pid
   keeps an instance wedged (`process_alive` treats `EPERM` as alive), and stop
   paths signal whatever process now has that pid.
2. The lock lives in `run_dir` but protects `instance_dir`; different
   `LNX_RUN_BASE` values give two independent locks over one instance.
3. A `final-snapshot.outcome status=pending` written by an owner that died before
   starting the VM wedges the instance until `snapshots clear`. Observed on macOS
   27 after a gvproxy bind failure.
4. Unix socket paths are not checked against `sun_path` (104 bytes). A long
   `LNX_BASE` yields `bind: invalid argument` from embedded gvproxy.
5. Snapshot durability ordering is inverted: the outcome record is fsynced, but
   snapshot data and the publish directory are not (unless `KRUN_SNAPSHOT_SYNC`).
   Publish is two renames with a window where `latest` is missing.
6. Disk/memory coherence of a snapshot is judged by mtimes and then forced by
   rewriting mtimes, not bound by a generation id.
7. Server UI status polling (every 3s) connects to the broker. That counts as
   activity, so the 5s idle timer never fires and the VM never snapshots.
8. Checkpoints run inside the broker accept loop. Concurrent clients time out on
   hello, conclude there is no broker, spawn a redundant owner, and fail with
   "owner exited with 0".
9. Live checkpoints copy `host-share-state` after vCPUs resumed; checkpoints are
   written in place (not atomic); `list()` accepts directories without
   `checkpoint.meta`; fork vs `checkpoints delete` is unlocked.
10. Three different stop protocols (`runner.rs` 120s, `cli.rs` 5s then SIGKILL
    which can kill mid-final-snapshot, `server.rs` 120s async poll).
11. Server import with `--replace`: unlocked probe, `rm -rf`, rename.
12. "Owner is stopping" is a plain-string error, not a typed retryable one.
13. Internal fork checkpoints are never removed.
14. Unlocked first-run init; fixed download temp file names.
15. `lnx.log` lines interleave (non-atomic appends from several writers).
16. `lnx --version` is forwarded to the guest as a command.
17. Instance VM config (cpus/memory/nested) is per-invocation; a snapshot taken with
    different flags makes plain `lnx <cmd>` hard-fail with "snapshot VM config
    mismatch".
18. Integration test runs leave instances in the real `~/.lnx`.

## Target architecture

- `instance::Lock`: one `flock` on `instance_dir/instance.lock`, held for the
  owner's lifetime, released by the kernel on death. Pids for display only.
- Typed instance state record committed atomically (tmp → fsync → rename →
  fsync dir), replacing inference from artifacts.
- Generation store: immutable `generations/<id>/` with a manifest; `latest` and
  checkpoints are refs; per-run work clones; GC by refcount.
- Owner as an explicit state machine (`OwnerPhase`) driven by one event loop;
  capture off the accept path; a hypervisor trait so the loop is unit-testable.
- Control RPC with typed, retryable errors; status probes not counted as activity.
- One service API shared by CLI and server (stop/status/checkpoint/fork/delete/import).
- TLA+ specs for owner handoff, snapshot commit/recovery, checkpoint/fork/delete.
- Crash-matrix tests with failpoints at every step.
