---------------------------- MODULE SnapshotCommit ----------------------------
(***************************************************************************)
(* CURRENT design of snapshot publication and restore-work recovery       *)
(* (main @ 077f0b09), for one instance whose owner already holds the lock *)
(* (OwnerLifecycle covers the lock). A sequence of owner runs; any run can *)
(* be killed (kill -9) or lose power at any step, after which the next    *)
(* run performs the code's recovery.                                       *)
(*                                                                         *)
(* Durability model ("journal prefix"):                                    *)
(*  - Metadata operations (create, clone, rename, unlink) are applied to   *)
(*    the namespace in order. After a power loss the namespace is the      *)
(*    state after SOME PREFIX of the operations since the last fsync.      *)
(*    Any fsync (file or directory) commits every earlier metadata         *)
(*    operation. This is how APFS/ext4 journals behave in practice and is  *)
(*    MORE forgiving than POSIX, so every violation found here is real.    *)
(*  - File data written without fsync is not durable: after power loss    *)
(*    the file holds an unspecified mix of old and new blocks (CORRUPT).   *)
(*    A clonefile() shares the source's blocks, so its data is exactly as  *)
(*    durable as the source's data was.                                    *)
(*  - kill -9 loses nothing on disk (dirty page cache, including the DAX   *)
(*    MAP_SHARED pages of the live rootfs, survives the process).          *)
(*                                                                         *)
(* Generations: run r produces guest state r. parent[r] records which     *)
(* state run r started from, so "contains acknowledged write a" is         *)
(* "a is an ancestor of the disk generation".                              *)
(***************************************************************************)
EXTENDS Naturals, Integers, Sequences, FiniteSets

CONSTANTS
    MaxRuns,       \* owner runs (including runs started after a fault)
    MaxFaults,     \* kill -9 + power-loss events
    SnapshotSync,  \* KRUN_SNAPSHOT_SYNC=1: fsync vmstate.bin/pages.img (container.rs:214, ram.rs:172)
    AllowPowerLoss

NOMEM == -1     \* object has no memory image (a bare rootfs)
CORRUPT == -2   \* unsynced data after power loss
Gens == 0..MaxRuns
MaxObj == 3 * MaxRuns + 2

\* Names: in memory-snapshots/ (latest, next = .latest.next, prev =
\* .latest.previous, work = .restore-work) and in the instance dir (canon =
\* rootfs.ext4, promote = .rootfs.ext4.promote).
Names == {"latest", "next", "prev", "work", "canon", "promote"}

VARIABLES
    obj,        \* object id -> [mem, memDur, disk, diskDur] ("Dur" = what survives power loss)
    nextObj,
    ns,         \* namespace: [Names -> object id or 0] @@ marker @@ outcome
    hist,       \* namespace states since the last commit; hist[1] is durable
    pc, run, restoring, parent, acked,
    faults,
    \* history flags checked by invariants
    silentAckLoss, ackLossOnClear, clearedGoodLatest, silentCorruptRestore, restoredFrom

vars == <<obj, nextObj, ns, hist, pc, run, restoring, parent, acked, faults,
          silentAckLoss, ackLossOnClear, clearedGoodLatest, silentCorruptRestore, restoredFrom>>

Ancestors(g) ==
    \* g and every generation it was derived from
    LET F[n \in 0..MaxRuns + 1] ==
          IF n = 0 THEN {g}
          ELSE LET prev == F[n - 1]
               IN prev \cup {parent[x] : x \in {y \in prev : y \in Gens /\ parent[y] \in Gens}}
    IN IF g \in Gens THEN F[MaxRuns + 1] ELSE {}

Contains(diskGen, a) == a \in Ancestors(diskGen)
AckedIn(diskGen) == \A a \in acked : Contains(diskGen, a)

EmptyNs == [latest |-> 0, next |-> 0, prev |-> 0, work |-> 0, canon |-> 0, promote |-> 0,
            marker |-> FALSE, outcome |-> [st |-> "none", run |-> 0]]

\* Apply a metadata operation (uncommitted).
MetaOp(newNs) == /\ ns' = newNs /\ hist' = Append(hist, newNs)
\* An fsync: commits every earlier metadata operation.
Commit(newNs) == /\ ns' = newNs /\ hist' = <<newNs>>

Set(n, v) == [ns EXCEPT ![n] = v]
Live == IF restoring THEN ns.work ELSE ns.canon
Clone(id) == obj[id]

-----------------------------------------------------------------------------
Init ==
    \* Instance with an existing, durable latest (gen 0) and canonical
    \* rootfs (gen 0): the steady state after a clean previous run.
    /\ obj = [i \in 1..MaxObj |-> IF i \in {1, 2}
                                    THEN [mem |-> IF i = 1 THEN 0 ELSE NOMEM,
                                          memDur |-> IF i = 1 THEN 0 ELSE NOMEM,
                                          disk |-> 0, diskDur |-> 0]
                                    ELSE [mem |-> NOMEM, memDur |-> NOMEM, disk |-> NOMEM, diskDur |-> NOMEM]]
    /\ nextObj = 3
    /\ ns = [EmptyNs EXCEPT !.latest = 1, !.canon = 2, !.outcome = [st |-> "success", run |-> 0]]
    /\ hist = <<ns>>
    /\ pc = "recover"
    /\ run = 1
    /\ restoring = FALSE
    /\ parent = [g \in Gens |-> -1]
    /\ acked = {}
    /\ faults = 0
    /\ silentAckLoss = FALSE
    /\ ackLossOnClear = FALSE
    /\ clearedGoodLatest = FALSE
    /\ silentCorruptRestore = FALSE
    /\ restoredFrom = -1

Alloc(rec) == /\ nextObj <= MaxObj
              /\ obj' = [obj EXCEPT ![nextObj] = rec]
              /\ nextObj' = nextObj + 1

-----------------------------------------------------------------------------
(* Recovery at owner start.                                               *)
(* validate_recovery_state_locked runner.rs:299-333 (outcome pending/error *)
(* or .restore-work + .restore-work.active => refuse until snapshots      *)
(* clear), then prepare_restore_for_start -> cleanup_snapshot_runtime_state*)
(* snapshots.rs:644-660 and cleanup_snapshot_publish_paths 679-711, then  *)
(* restore from latest if present (refresh_default_restore_snapshot       *)
(* runner.rs:698-718), else cold boot on the canonical rootfs.             *)
Recover ==
    /\ pc = "recover"
    /\ run <= MaxRuns
    /\ IF ns.outcome.st \in {"pending", "error"} \/ (ns.marker /\ ns.work # 0)
         THEN /\ pc' = "wedged"
              /\ UNCHANGED <<ns, hist, restoring, silentAckLoss, silentCorruptRestore,
                             restoredFrom, parent>>
         ELSE LET n1 == IF ns.marker /\ ns.work = 0 THEN [ns EXCEPT !.marker = FALSE] ELSE ns
                  n2 == [n1 EXCEPT !.work = 0]
                  n3 == IF n2.prev # 0 /\ n2.latest = 0
                          THEN [n2 EXCEPT !.latest = n2.prev, !.prev = 0] ELSE n2
                  n4 == [n3 EXCEPT !.next = 0, !.prev = 0]
                  src == IF n4.latest # 0 THEN n4.latest ELSE n4.canon
                  memOk == obj[src].mem # CORRUPT
              IN /\ MetaOp(n4)
                 /\ restoring' = (n4.latest # 0)
                 /\ restoredFrom' = obj[src].disk
                 /\ parent' = [parent EXCEPT ![run] = obj[src].disk]
                 \* A torn pages.img with an intact vmstate.bin header is not
                 \* detected (no checksum); a torn vmstate.bin is refused.
                 /\ \E detected \in BOOLEAN :
                      IF n4.latest # 0 /\ ~memOk
                        THEN IF detected
                               THEN pc' = "refused" /\ UNCHANGED silentCorruptRestore
                               ELSE pc' = "start" /\ silentCorruptRestore' = TRUE
                        ELSE pc' = "start" /\ UNCHANGED silentCorruptRestore
                 /\ silentAckLoss' = (silentAckLoss \/ ~AckedIn(obj[src].disk))
    /\ UNCHANGED <<obj, nextObj, run, acked, faults, ackLossOnClear, clearedGoodLatest>>

\* `lnx snapshots clear` (cli.rs:2646-2719): the only remedy for a wedged or
\* refused instance. Detaches latest/.next/.previous/.restore-work/marker,
\* acknowledges the outcome, fsyncs memory-snapshots/; the next run cold
\* boots on the canonical rootfs.
Clear ==
    /\ pc \in {"wedged", "refused"}
    /\ Commit([ns EXCEPT !.latest = 0, !.next = 0, !.prev = 0, !.work = 0,
                         !.marker = FALSE, !.outcome = [st |-> "cleared", run |-> run]])
    /\ ackLossOnClear' = (ackLossOnClear \/ ~AckedIn(obj[ns.canon].disk))
    \* latest was a complete, coherent snapshot holding every acknowledged
    \* write, and the canonical rootfs did not: the wedge was spurious and
    \* its only remedy destroyed the good copy.
    /\ clearedGoodLatest' = (clearedGoodLatest \/
          (/\ ns.latest # 0
           /\ obj[ns.latest].mem = obj[ns.latest].disk
           /\ AckedIn(obj[ns.latest].disk)
           /\ ~AckedIn(obj[ns.canon].disk)))
    \* The user explicitly accepted losing whatever canon lacks.
    /\ acked' = {a \in acked : Contains(obj[ns.canon].disk, a)}
    /\ pc' = "recover"
    /\ UNCHANGED <<obj, nextObj, run, restoring, parent, faults,
                   silentAckLoss, silentCorruptRestore, restoredFrom>>

-----------------------------------------------------------------------------
(* One owner run (start_vm runner.rs:784-1271, run_broker_owner final      *)
(* snapshot 3056-3133, serve_snapshot 4462-4551).                          *)

Step(from, to) == pc = from /\ pc' = to

\* clone_restore_snapshot snapshots.rs:601-620 (clonefile, no fsync).
CloneWork ==
    /\ Step("start", IF restoring THEN "mark" ELSE "pending")
    /\ IF restoring
         THEN /\ Alloc(Clone(ns.latest))
              /\ MetaOp(Set("work", nextObj))
         ELSE UNCHANGED <<obj, nextObj, ns, hist>>
    /\ UNCHANGED <<run, restoring, parent, acked, faults, silentAckLoss,
                   ackLossOnClear, clearedGoodLatest, silentCorruptRestore, restoredFrom>>

\* mark_restore_work_active runner.rs:1187 (fs::write, no fsync).
Mark ==
    /\ Step("mark", "pending")
    /\ MetaOp([ns EXCEPT !.marker = TRUE])
    /\ UNCHANGED <<obj, nextObj, run, restoring, parent, acked, faults,
                   silentAckLoss, ackLossOnClear, clearedGoodLatest, silentCorruptRestore, restoredFrom>>

\* write_final_snapshot_pending runner.rs:1197: tmp, fsync, rename, fsync dir
\* (snapshots.rs:392-442).
Pending ==
    /\ Step("pending", "run")
    /\ Commit([ns EXCEPT !.outcome = [st |-> "pending", run |-> run]])
    /\ UNCHANGED <<obj, nextObj, run, restoring, parent, acked, faults,
                   silentAckLoss, ackLossOnClear, clearedGoodLatest, silentCorruptRestore, restoredFrom>>

\* The guest writes its disk (DAX pages of the live rootfs). It may issue
\* a flush (virtio-pmem FLUSH -> msync + fsync, pmem/device.rs:81-102),
\* after which the write is acknowledged to the user.
GuestRun ==
    /\ Step("run", "pause")
    /\ \E flush \in BOOLEAN :
         /\ obj' = [obj EXCEPT ![Live] = [@ EXCEPT !.disk = run,
                                                  !.diskDur = IF flush THEN run ELSE @]]
         /\ acked' = IF flush THEN acked \cup {run} ELSE acked
         /\ IF flush THEN Commit(ns) ELSE UNCHANGED <<ns, hist>>
    /\ UNCHANGED <<nextObj, run, restoring, parent, faults, silentAckLoss,
                   ackLossOnClear, clearedGoodLatest, silentCorruptRestore, restoredFrom>>

\* pmem flush before pause (pmem/device.rs:256-260), then capture.
\* seed_incremental_snapshot runner.rs:4588-4627 clones latest into .next.
PauseAndSeed ==
    /\ Step("pause", "capture")
    /\ LET flushed == [obj EXCEPT ![Live] = [@ EXCEPT !.diskDur = obj[Live].disk]]
           seed == IF restoring THEN obj[ns.latest]
                   ELSE [mem |-> NOMEM, memDur |-> NOMEM, disk |-> NOMEM, diskDur |-> NOMEM]
       IN /\ nextObj <= MaxObj
          /\ obj' = [flushed EXCEPT ![nextObj] = seed]
          /\ nextObj' = nextObj + 1
          /\ Commit(Set("next", nextObj))   \* the flush's fsync commits; the seed clone is after it
    /\ UNCHANGED <<run, restoring, parent, acked, faults, silentAckLoss,
                   ackLossOnClear, clearedGoodLatest, silentCorruptRestore, restoredFrom>>

\* snapshot_with_file_copy (capture_snapshot_for_publish runner.rs:4575):
\* vmstate.bin/pages.img written, fsynced only with KRUN_SNAPSHOT_SYNC;
\* rootfs.ext4 cloned from the (flushed) live rootfs.
Capture ==
    /\ Step("capture", "mv1")
    /\ obj' = [obj EXCEPT ![ns.next] = [mem |-> run,
                                         memDur |-> IF SnapshotSync THEN run ELSE CORRUPT,
                                         disk |-> obj[Live].disk,
                                         diskDur |-> obj[Live].diskDur]]
    /\ IF SnapshotSync THEN Commit(ns) ELSE MetaOp(ns)
    /\ UNCHANGED <<nextObj, run, restoring, parent, acked, faults, silentAckLoss,
                   ackLossOnClear, clearedGoodLatest, silentCorruptRestore, restoredFrom>>

\* publish_snapshot_dir snapshots.rs:815-863: rename latest -> .latest.previous,
\* rename .latest.next -> latest, remove .latest.previous. No fsync.
Mv1 ==
    /\ Step("mv1", "mv2")
    /\ MetaOp(IF ns.latest # 0 THEN [ns EXCEPT !.prev = ns.latest, !.latest = 0] ELSE ns)
    /\ UNCHANGED <<obj, nextObj, run, restoring, parent, acked, faults,
                   silentAckLoss, ackLossOnClear, clearedGoodLatest, silentCorruptRestore, restoredFrom>>
Mv2 ==
    /\ Step("mv2", "rmPrev")
    /\ MetaOp([ns EXCEPT !.latest = ns.next, !.next = 0])
    /\ UNCHANGED <<obj, nextObj, run, restoring, parent, acked, faults,
                   silentAckLoss, ackLossOnClear, clearedGoodLatest, silentCorruptRestore, restoredFrom>>
RmPrev ==
    /\ Step("rmPrev", IF restoring THEN "promoteClone" ELSE "clearMarker")
    /\ MetaOp([ns EXCEPT !.prev = 0])
    /\ UNCHANGED <<obj, nextObj, run, restoring, parent, acked, faults,
                   silentAckLoss, ackLossOnClear, clearedGoodLatest, silentCorruptRestore, restoredFrom>>

\* promote_snapshot_rootfs snapshots.rs:761-813 (only for restored runs,
\* runner.rs:1173-1177): clone latest/rootfs.ext4 -> .rootfs.ext4.promote,
\* rename over rootfs.ext4. No fsync.
PromoteClone ==
    /\ Step("promoteClone", "promoteMv")
    /\ Alloc([mem |-> NOMEM, memDur |-> NOMEM,
              disk |-> obj[ns.latest].disk, diskDur |-> obj[ns.latest].diskDur])
    /\ MetaOp(Set("promote", nextObj))
    /\ UNCHANGED <<run, restoring, parent, acked, faults, silentAckLoss,
                   ackLossOnClear, clearedGoodLatest, silentCorruptRestore, restoredFrom>>
PromoteMv ==
    /\ Step("promoteMv", "clearMarker")
    /\ MetaOp([ns EXCEPT !.canon = ns.promote, !.promote = 0])
    /\ UNCHANGED <<obj, nextObj, run, restoring, parent, acked, faults,
                   silentAckLoss, ackLossOnClear, clearedGoodLatest, silentCorruptRestore, restoredFrom>>

\* finish_restore_work_after_final_snapshot runner.rs:3095, snapshots.rs:301-308.
ClearMarker ==
    /\ Step("clearMarker", "outcome")
    /\ MetaOp([ns EXCEPT !.marker = FALSE])
    /\ UNCHANGED <<obj, nextObj, run, restoring, parent, acked, faults,
                   silentAckLoss, ackLossOnClear, clearedGoodLatest, silentCorruptRestore, restoredFrom>>

\* write_final_snapshot_outcome runner.rs:580 (tmp, fsync, rename, fsync dir).
Outcome ==
    /\ Step("outcome", "recover")
    /\ Commit([ns EXCEPT !.outcome = [st |-> "success", run |-> run]])
    /\ run' = run + 1
    /\ UNCHANGED <<obj, nextObj, restoring, parent, acked, faults, silentAckLoss,
                   ackLossOnClear, clearedGoodLatest, silentCorruptRestore, restoredFrom>>

RunPCs == {"start", "mark", "pending", "run", "pause", "capture", "mv1", "mv2",
           "rmPrev", "promoteClone", "promoteMv", "clearMarker", "outcome"}

\* kill -9 of the owner at any step: the disk keeps everything.
Kill ==
    /\ faults < MaxFaults
    /\ pc \in RunPCs
    /\ pc' = "recover"
    /\ run' = run + 1
    /\ faults' = faults + 1
    /\ UNCHANGED <<obj, nextObj, ns, hist, restoring, parent, acked, silentAckLoss,
                   ackLossOnClear, clearedGoodLatest, silentCorruptRestore, restoredFrom>>

\* Power loss at any step: the namespace reverts to a prefix of the
\* operations since the last commit; unsynced file data is lost.
PowerLoss ==
    /\ AllowPowerLoss
    /\ faults < MaxFaults
    /\ pc \in RunPCs \cup {"recover"}
    /\ \E k \in 1..Len(hist) :
         /\ ns' = hist[k]
         /\ hist' = <<hist[k]>>
    /\ obj' = [i \in 1..MaxObj |-> [obj[i] EXCEPT !.mem = obj[i].memDur, !.disk = obj[i].diskDur]]
    /\ pc' = "recover"
    /\ run' = IF pc = "recover" THEN run ELSE run + 1
    /\ faults' = faults + 1
    /\ UNCHANGED <<nextObj, restoring, parent, acked, silentAckLoss, ackLossOnClear, clearedGoodLatest,
                   silentCorruptRestore, restoredFrom>>

Next ==
    \/ Recover \/ Clear
    \/ CloneWork \/ Mark \/ Pending \/ GuestRun \/ PauseAndSeed \/ Capture
    \/ Mv1 \/ Mv2 \/ RmPrev \/ PromoteClone \/ PromoteMv \/ ClearMarker \/ Outcome
    \/ Kill \/ PowerLoss

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------
(******************************** PROPERTIES *******************************)

Durable == hist[1]

\* A durable outcome "success" for run r implies the durable latest is run
\* r's snapshot with durable memory and disk.
SuccessImpliesDurable ==
    LET d == Durable IN
    (d.outcome.st = "success" /\ d.outcome.run > 0) =>
        /\ d.latest # 0
        /\ obj[d.latest].memDur = d.outcome.run
        /\ obj[d.latest].diskDur = d.outcome.run

\* Memory and disk of latest belong to the same generation.
LatestCoherent ==
    ns.latest # 0 /\ pc \notin {"capture"} =>
        obj[ns.latest].mem \in {obj[ns.latest].disk, CORRUPT}

\* No restore from a memory image that is silently corrupt.
NoSilentCorruptRestore == ~silentCorruptRestore

\* Recovery never continues from a state that lacks an acknowledged write
\* without the user being asked.
NoSilentAckLoss == ~silentAckLoss

\* Even the destructive remedy (`snapshots clear`) does not lose
\* acknowledged writes: clear only drops memory.
NoAckLossOnClear == ~ackLossOnClear

\* `snapshots clear` is never forced on an instance whose latest is a
\* complete snapshot that holds acknowledged writes the canonical rootfs
\* lacks (crash after publish, before .restore-work.active is removed).
NoClearOfGoodLatest == ~clearedGoodLatest

=============================================================================
