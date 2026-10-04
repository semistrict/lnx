# TLA+ models of lnx's instance protocols

These models check the three protocols where lnx keeps breaking (see
`docs/research/reliability-audit.md`): who owns an instance, how a snapshot
becomes `latest`, and how checkpoints interact with fork and delete. Each
protocol has two models:

- the **current** design, faithful to `main` @ 077f0b09 (every action cites
  `file:line`), whose configs reproduce real bugs, and
- a **target** design that the refactor must implement, which passes.

Run everything with:

```sh
bun run tla:check            # or scripts/tla/check.sh [cfg-path-filter]
TLA_KEEP_LOGS=/tmp/tlc bun run tla:check   # keep counterexample traces
```

The script downloads tla2tools.jar v1.7.4 (MIT, latest stable release) into
`${XDG_CACHE_HOME:-~/.cache}/lnx/`, checks its pinned sha256, and runs TLC on
every `.cfg` under `specs/tla/`. The full suite (42 configs) takes about one
minute.

## Config convention

`<Dir>/<Module>.cfg` and `<Dir>/<Module>.<Variant>.cfg` are TLC configs for
`<Dir>/<Module>.tla`. The first line of every config states what TLC must
report:

- `\* EXPECT: pass`: no invariant or property may fail.
- `\* EXPECT: violation <Invariant>`: TLC must report exactly
  `Invariant <Invariant> is violated`.

`check.sh` exits non-zero if any config misses its expectation: a target model
fails, a current-design config stops reproducing its bug, or a model does not
parse. Three kinds of configs use `violation`:

- current-design bug reproductions (`OwnerLifecycle.NoStuckInstance.cfg`, …).
  When the refactor lands, delete them together with the old code;
- `Witness*`: non-vacuity checks. Each shows that an interesting state of a
  target model is reachable, so the passing invariants are not passing
  because nothing happens;
- `Mut_*`: mutation checks. Each removes one target rule and shows that an
  invariant then fails, so the rule is necessary.

## Results

| Config | Distinct states | Result |
| --- | ---: | --- |
| `OwnerLifecycle.cfg` (2 clients, 1 crash, 1 pid reuse, stop) | 646,652 | pass: AtMostOneOwner, AtMostOneVm, NoExecOnDraining, NoDoubleExec |
| `OwnerLifecycle.*.cfg` (10 configs) | n/a | each invariant below violated (expected) |
| `OwnerLifecycleTarget.cfg` (2 clients, 1 crash, stop, recover) | 292,668 | pass: all 12 invariants + LatestMonotonic |
| `OwnerLifecycleTarget.TwoCrashes.cfg` (2 crashes, 3 spawns per client) | 1,362,826 | pass |
| `SnapshotCommit.cfg` (3 runs, 2 faults, power loss) | 18,783 | pass: NoSilentAckLoss, LatestCoherent |
| `SnapshotCommit.*.cfg` (4 configs) | n/a | violated (expected) |
| `SnapshotCommitTarget.cfg` (3 runs, 2 faults incl. power loss) | 8,073 | pass: all 9 invariants |
| `SnapshotCommitTarget.Mut_*.cfg` (4 configs) | n/a | each removed rule breaks an invariant |
| `CheckpointForkDelete.*.cfg` (5 configs) | n/a | violated (expected) |
| `CheckpointForkDeleteTarget.cfg` (1 crash, 2 generations) | 2,564 | pass: all 5 invariants |
| `CheckpointForkDeleteTarget.Mut_*.cfg` (2 configs) | n/a | each removed rule breaks ForkIsComplete |

---

## 1. OwnerLifecycle

`OwnerLifecycle/OwnerLifecycle.tla` models N CLI invocations (`runner::run`),
the `_vm-owner` process each one may spawn, `owner-start.lock.d` and
`bootstrap.lock.d` (pid-file directories with stale detection through
`kill(pid, 0)`, which also treats `EPERM` as alive), the broker socket, the
idle timer, the drain barrier, the final-snapshot outcome marker, the
restore-work marker, a `stop`/`delete` caller, kill -9 of any process at any
step (including inside the lock-dir guard), and pid reuse as an environment
action. Work done under the lock-dir guard flock is one atomic step;
`OCrashMidAcquire` / `OCrashMidRelease` model a kill between `mkdir` and
writing `owner.pid` (and the reverse on release).

Not modeled: timeouts (the model never gives up waiting, so liveness is not
checked), the 1 s hello timeout of a connection that sits in the listen
backlog (it behaves like "no broker"), `InstanceStateLock`, checkpoint
requests, and split `LNX_RUN_BASE` lock directories (audit item 2).

Invariants: `AtMostOneOwner`, `AtMostOneVm`, `NoExecOnDraining`,
`NoDoubleExec` (all hold), and the ones below (all violated).

### Bugs found in the current design

1. **Spurious wedge: "outcome=pending" from an owner whose VM never ran**
   (`NoWedgeFromPendingWithoutVm`, reachable with **zero** crashes; this is the
   bug observed on macOS 27). The owner acquires `bootstrap.lock.d`, clones
   latest into `.restore-work`, writes `.restore-work.active`, then writes
   `final-snapshot.outcome status=pending` (runner.rs:1197) *before*
   `vm.start()`. krun fails before any guest code runs (or the agent never
   connects), `run_broker_owner` returns `Err`, `run_owner` returns the error
   (runner.rs:573) and the lock is released with no outcome written. Every later
   command fails "did not finish reporting its final snapshot … snapshots
   clear", and `snapshots clear` deletes a perfectly good `latest`.
2. **Spurious wedge: lease/outcome mismatch** (`NoWedgeFromLeaseOutcomeMismatch`).
   An owner is killed right after taking `bootstrap.lock.d`, before writing
   anything else. The dead lease names pid P while the outcome file still names
   the previous owner: "reported a final snapshot outcome for pid X, expected
   pid P" until `snapshots clear`.
3. **Spurious wedge: pid-less lease** (`NoWedgeFromPidlessLease`). A kill between
   `create_dir` and `write_owner_lease` (locks.rs:184-185), or between removing
   `owner.pid` and `remove_dir` on release (locks.rs:295-298), leaves an empty
   lock dir: "incomplete owner lease … snapshots clear".
4. **Spurious wedge: stale restore marker** (`NoWedgeFromStaleRestoreMarker`).
   `.restore-work.active` is written before `vm.start()` and removed only after
   publish and promotion; a kill anywhere in that window (before the VM ran, or
   after `latest` was already published) refuses with "a previous restored VM
   did not publish a final snapshot". (`SnapshotCommit` shows this remedy also
   destroys acknowledged writes.)
5. **Stuck instance after pid reuse** (`NoStuckInstance`). A client or owner is
   killed while holding a lock dir and its pid is reused by any process (even
   another user's: `EPERM` counts as alive). The lock is never stale again:
   every client waits 120 s and fails with "timed out waiting for …lock.d", and
   `snapshots clear` refuses because the instance "has a running VM owner".
   There is no recovery short of deleting the directory by hand.
6. **Stop/delete signals an unrelated process** (`NoSignalForeign`). Same setup:
   `lnx delete` (cli.rs:1390-1405, SIGTERM then SIGKILL to the process group)
   or the server's stop (server.rs:1675-1680) signals whatever now owns the
   recorded pid.
7. **In-flight command frozen into the snapshot** (`NoOrphanAtSnapshot`). A stop
   arrives while a command runs: `begin_broker_shutdown` sends the client
   "VM owner is stopping" but leaves the guest command running, and the agent
   handles `SnapshotReady` without stopping children (guest-agent main.rs:2901).
   The command is captured mid-flight and resumes after the next restore with no
   client attached; a user who retries runs it twice.
8. **Client fails because its owner idled out first**
   (`NoFailureFromIdleExitBeforeAttach`). The daemon owner starts idle
   (`starts_idle = true`, runner.rs:560-563). If its idle TTL passes before the
   spawning client connects, the owner snapshots and exits 0, and the client
   reports "lnx VM owner exited with exit status: 0 before the broker came up".
9. **Client fails because a broker existed** (`NoFailureFromExistingBrokerExit`,
   audit item 8). Client B waits for the start lock while A runs a command,
   takes it when A finishes, and spawns owner B (it does not re-check the
   broker). Owner B sees A's broker and exits 0 "existing_broker"
   (runner.rs:546-548); A's owner then idles out; B cannot connect, sees its
   child exited, and fails the same way.

## 2. SnapshotCommit

`SnapshotCommit/SnapshotCommit.tla` models a sequence of owner runs on one
instance (the lock is assumed held): recovery at start
(`validate_recovery_state_locked`, `cleanup_snapshot_runtime_state`,
`cleanup_snapshot_publish_paths`), the `.restore-work` clone, the marker, the
pending outcome, guest writes and flushes (virtio-pmem FLUSH = msync + fsync,
third_party/libkrun/src/devices/src/virtio/pmem/device.rs:81-102), the pmem
flush before pause, seeding `.latest.next`, capture, the two-rename publish,
rootfs promotion, marker removal, the success outcome, and `snapshots clear`
as the remedy. kill -9 and power loss can strike at every step.

Durability model ("journal prefix"): metadata operations are ordered; after a
power loss the namespace equals the state after some prefix of the operations
since the last fsync, and any fsync commits all earlier metadata. File data not
fsynced is lost (`CORRUPT`); a `clonefile` is exactly as durable as its source.
This is more forgiving than POSIX (which requires a directory fsync per
directory), so every violation is real on APFS. Generations carry a parent so
"contains acknowledged write a" is ancestry.

Invariants: `NoSilentAckLoss` and `LatestCoherent` hold (the pending marker
stops every unsafe automatic path). Violated:

1. **outcome=success before latest is durable** (`SuccessImpliesDurable`).
   `write_final_snapshot_outcome` fsyncs the outcome and the directory, but
   `vmstate.bin`/`pages.img` are fsynced only with `KRUN_SNAPSHOT_SYNC=1`
   (container.rs:214, ram.rs:172), which is off by default.
2. **Silent restore of a torn memory image** (`NoSilentCorruptRestore`). Power
   loss after a successful run: the outcome and the `latest` rename are durable,
   `pages.img` is not. Nothing checksums it; a torn `pages.img` with an intact
   `vmstate.bin` header restores corrupt guest memory.
3. **The only remedy destroys acknowledged writes** (`NoAckLossOnClear`, no
   power loss needed). kill -9 of a restored run after the guest flushed a write
   (`fsync` in the guest returned, the user saw success): the instance is
   wedged (pending + marker) and `snapshots clear` deletes `.restore-work`, the
   only copy of that write, then cold-boots the canonical rootfs.
4. **Clear destroys a good latest** (`NoClearOfGoodLatest`, no power loss
   needed). kill -9 after `rename(.latest.next, latest)` and before the marker
   is removed: `latest` is complete and holds every acknowledged write, the
   canonical `rootfs.ext4` has not been promoted yet, and recovery refuses.
   `snapshots clear` deletes `latest` and cold-boots the stale rootfs.

## 3. CheckpointForkDelete

`CheckpointForkDelete/CheckpointForkDelete.tla` models one checkpoint id with
file groups (memory, rootfs, stamps, host-share-state, manifest,
`checkpoint.meta`), both creation paths (a live owner writing in place,
runner.rs:2849-2930; a foreground run that publishes `.<id>.next` and then
writes `checkpoint.meta` after dropping the owner lock), `checkpoints list`,
`fork <id>` (validate, then clone file by file, skipping missing files), and
`checkpoints delete` (`remove_dir_all`, refused only while an owner lock is
held). Violated:

1. **Fork during creation** (`ForkDuringCreate`): `seed_incremental_snapshot`
   first clones the old `latest` into `checkpoints/<id>/`, so validation passes
   immediately; the fork copies stale and half-written files (new memory, old
   rootfs).
2. **Fork during delete** (`ForkDuringDelete`): fork takes no lock; delete
   removes `vmstate.bin` after validation; `clone_snapshot_dir` skips the
   missing file (checkpoints.rs:504) and the broken fork is published.
3. **Fork after a killed creator** (`ForkAfterCrash`): nothing removes the
   half-written directory; it stays listed and forkable (new memory paired with
   the old rootfs).
4. **Delete between publish and checkpoint.meta** (`DeleteBeforeMeta`):
   `write_metadata` uses `create_dir_all`, recreates the deleted directory with
   only `checkpoint.meta`, and `lnx checkpoint` reports success.
5. **List shows incomplete checkpoints** (`ListDuringCreate`): `list()`
   returns every directory, including in-progress ones and the dot-named
   `.<id>.next` temp directory.

---

## Target design rules

These are the rules the Rust refactor must follow. Each one is in a target
model; the `Mut_*` configs show which ones fail when removed. "Commit a file"
always means: write `<name>.tmp-<random>` in the same directory with
`O_CREAT|O_EXCL`, `sync_all()` (Rust uses `F_FULLFSYNC` on macOS), `rename`
over `<name>`, then `sync_all()` on the directory.

The target models compose: `OwnerLifecycleTarget` treats capture + commit as
one step and `SnapshotCommitTarget` refines that step; `CheckpointForkDeleteTarget`
reuses the generation store. Limits shared by all models: bounded instances
(2 clients, 1-2 crashes, 2-3 runs, one checkpoint name), and no liveness
checking (timeouts are not modeled).

### Owner rules (OwnerLifecycleTarget.tla)

- **O1 One lock.** `instance_dir/instance.lock` (in the persistent instance
  dir, so `LNX_RUN_BASE` cannot split it), `flock(LOCK_EX|LOCK_NB)`. The
  client that wants to start an owner takes it, runs recovery (O3) while
  holding it, then spawns `_vm-owner` with the locked fd inherited (cleared
  `FD_CLOEXEC` on that fd only) and closes its own copy. The owner holds it for
  its whole life; the kernel releases it on any exit. The lock file is never
  unlinked. `owner-start.lock.d`, `bootstrap.lock.d`, their `.guard` files and
  all pid-liveness checks are deleted. Every state-mutating command (`delete`,
  `recover`, `snapshots …`, checkpoint GC, `import --replace`) takes the same
  flock.
- **O2 One instance record.** `instance_dir/state`, committed as above, written
  only by the lock holder: `{version, latest_generation, phase, run_id,
  owner: {pid, start_time}}`.
  - `phase = running`: committed by the owner after `runs/<run_id>/` exists and
    is fsynced (S2), immediately before `vm.start()`.
  - `phase = dirty`: committed by the owner while holding the broker clients
    mutex, before forwarding the first `OpenExec`/`OpenTcp` of the run (once
    per owner run; two fsyncs on the first command of a burst).
  - `phase = stopped` together with the new `latest_generation`: the snapshot
    commit (S5), one replacement.
  It replaces `final-snapshot.outcome`, `.restore-work.active`, and every
  "infer state from artifacts" check.
- **O3 Recovery** (lock holder, before spawning; and `lnx recover`):
  - `stopped`: GC (S6), continue.
  - `running` or `dirty` and `generations/<gen of run_id>` exists with a valid
    manifest: roll forward (commit `{latest: that gen, phase: stopped}`), GC,
    continue.
  - `running` (nothing was dispatched): commit `stopped`, GC (deletes
    `runs/<run_id>`), continue without asking.
  - `dirty`: fail with typed `CrashedWithUnsavedState{run_id}`. Remedies:
    `lnx recover --salvage` (commit `runs/<run_id>/rootfs.ext4` as a disk-only
    generation, keeping every acknowledged write) or `lnx recover --discard-run`
    (commit `stopped`, GC the run). Neither ever deletes `latest`.
- **O4 Owner start failure** (before serving): commit `stopped` (latest
  unchanged), delete `runs/<run_id>`, exit with a typed boot-failure code; the
  spawning client reports that error.
- **O5 No client failure on races; at-most-once execution.** The owner answers
  an open with typed `Retry(Stopping)` during an idle drain and
  `Stopped` during an explicit stop, both before dispatch. The client retries
  from the top (connect, else lock and spawn, else wait) when: connect or hello
  fails, the broker closes before the client wrote its `OpenExec`, or it gets
  `Retry`. Once `OpenExec` was written and not refused, it never retries; a
  vanished owner is reported as "owner died". There is no separate start lock
  and no "existing broker" exit: only the lock holder spawns an owner.
- **O6 Idle accounting.** The idle timer may fire only when there is no
  registered channel, no accepted-but-unregistered connection, and no spawn
  reservation. The spawning client passes a pipe to the owner; the reservation
  ends at that client's first open or at EOF on the pipe. Status probes use an
  RPC that does not count as activity.
- **O7 Stop drain.** Stop sets `stopping` under the clients mutex, then sends
  the guest `Terminate(channel)` for every in-flight channel (SIGTERM to the
  process group, SIGKILL after a grace period), waits for each `ExitStatus`
  and delivers it, and only then sends `SnapshotReady`. The guest agent refuses
  `SnapshotReady` while it still has exec children.
- **O8 No signals by pid.** `stop`/`delete` send `Stop` over the owner's
  control socket. A forced kill is allowed only when `LOCK_NB` on
  `instance.lock` fails and the pid's current start time
  (`proc_pidinfo(PROC_PIDTBSDINFO)`) equals `owner.start_time` in `state`.

### Snapshot rules (SnapshotCommitTarget.tla)

- **S1 Layout.** `generations/<gen>/` is immutable once renamed into place:
  `vmstate.bin`, `pages.img`, `rootfs.ext4`, stamps, `host-share-state/`,
  `manifest.json` (gen id, parent gen, run id, size of every file, sha256 of
  the small files; the gen id is also written into the vmstate header so a
  file from another generation is rejected). `runs/<run_id>/` is the live work
  dir. There is no canonical `rootfs.ext4`, no `latest/`, `.latest.next/`,
  `.latest.previous/`, `.restore-work/`, and no mtime-based coherence check or
  mtime rewriting.
- **S2 Run start.** Clone `generations/<latest>/` files into
  `runs/<run_id>/`, fsync each file, `runs/<run_id>/` and `runs/`; then commit
  `phase = running` (O2).
- **S3** Commit `phase = dirty` before the first dispatch (O2).
  `Mut_noDirtyPhase`: without it a crash after an acknowledged write is
  auto-recovered from `latest` and the write is gone.
- **S4 Capture.** pmem flush (msync + fsync of the live rootfs) before pausing
  (`Mut_noPauseFlush`: power loss pairs new memory with an older disk). Into
  `generations/.staging-<gen>/`: clone the parent's `pages.img` and write the
  dirty pages, write `vmstate.bin`, clone `runs/<run_id>/rootfs.ext4`, copy
  stamps and host-share-state, write the manifest; fsync every file and the
  staging dir (`Mut_noStageFsync`: the record can name a non-durable
  generation). Rename `.staging-<gen>` to `<gen>`, fsync `generations/`.
- **S5 Commit point.** Commit `state = {latest_generation: gen, phase:
  stopped}`. Before this, recovery restores the old generation (or rolls this
  one forward, O3); after it, the new one. Nothing else decides.
- **S6 GC, after S5 only** (and at recovery, under the lock): delete
  `runs/*`, `generations/.staging-*`, and every generation that neither `state`
  nor a checkpoint ref names and whose `.pin` can be locked (C5). No fsync is
  needed: a resurrected entry is garbage again. `Mut_gcBeforeCommit`: GC before
  the commit deletes the generation the commit is about to name.
- **S7** `lnx snapshots clear` becomes "drop memory": commit a disk-only
  generation cloned from the latest rootfs (the same mechanism as
  `recover --salvage`, which the model checks). It never discards disk state.

### Checkpoint rules (CheckpointForkDeleteTarget.tla)

- **C1** A checkpoint is `checkpoints/<name>.ref` holding a generation id,
  committed (as above) only after the generation was published (S4) under the
  instance lock. `lnx checkpoint` reports success only after that commit.
  Internal fork checkpoints use the same path and are deleted after the fork.
- **C2** `checkpoints list` reads only `*.ref` files whose generation exists.
- **C3** `checkpoints delete` unlinks the ref; it never touches the generation.
- **C4** `fork` resolves the ref, takes `flock(LOCK_SH)` on
  `generations/<gen>/.pin`, then checks `generations/<gen>` is still in place
  (fail with "checkpoint deleted" otherwise), copies, publishes the fork, and
  releases the pin. `Mut_noPin` and `Mut_noRecheck` show both halves are
  needed.
- **C5** GC deletes a generation only if no ref and no `state` names it and
  `flock(LOCK_EX|LOCK_NB)` on its `.pin` succeeds; it renames the directory to
  trash before the recursive delete.
- **C6** Checkpoint capture runs under the instance lock (owner or foreground
  run), so GC never sees a half-made checkpoint; a crash leaves only staging
  garbage for the next GC.
