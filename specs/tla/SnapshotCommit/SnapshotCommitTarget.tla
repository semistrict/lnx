------------------------- MODULE SnapshotCommitTarget -------------------------
(***************************************************************************)
(* TARGET snapshot store: immutable generations and one atomically         *)
(* replaced instance record. Same durability model and faults as           *)
(* SnapshotCommit.tla (journal prefix for metadata, unsynced file data is  *)
(* lost on power loss, kill -9 loses nothing on disk). Rules, numbered as  *)
(* in specs/tla/README.md ("Snapshot rules"):                              *)
(*                                                                         *)
(* S1 Layout: instance_dir/state (the record), generations/<gen>/ (immutable*)
(*    once renamed into place: vmstate.bin, pages.img, rootfs.ext4, stamps, *)
(*    manifest.json with gen id, parent gen, run id, origin, size+checksum  *)
(*    of every file), runs/<run>/ (live work dir), .staging-<gen>/.         *)
(*    No canonical rootfs: a cold boot uses generations/<latest>/rootfs.ext4.*)
(* S2 Run start: clone the latest generation into runs/<run>/, fsync every  *)
(*    file and runs/<run>/ and runs/, THEN commit state {phase: running}.   *)
(* S3 Before the first command is dispatched: commit state {phase: dirty}.  *)
(* S4 Capture: pmem flush (msync+fsync), write vmstate/pages into           *)
(*    .staging-<gen>/, clone rootfs, write manifest; fsync every file and   *)
(*    the staging dir. rename .staging-<gen> -> generations/<gen>; fsync    *)
(*    generations/.                                                         *)
(* S5 Commit point: state := {latest: gen, phase: stopped} via tmp, fsync,  *)
(*    rename, fsync instance_dir. Nothing before this point changes what    *)
(*    recovery restores; everything after it is garbage collection.         *)
(* S6 GC (after S5 and at recovery): delete runs/*, .staging-*, and every   *)
(*    generation not referenced by state.latest or a checkpoint ref. GC     *)
(*    needs no fsync: a resurrected garbage entry is collected again.       *)
(* S7 Recovery: phase stopped|running -> GC, restore state.latest (verify   *)
(*    manifest checksums first); phase dirty -> CrashedWithUnsavedState:    *)
(*    `lnx recover --salvage` turns runs/<run>/rootfs.ext4 into a disk-only *)
(*    generation via S4+S5 (keeps every acknowledged write, drops memory);  *)
(*    `lnx recover --discard-run` commits phase stopped and GCs the run     *)
(*    (explicit consent to drop it). Neither touches state.latest until its *)
(*    own S5 commit. If the crashed run's FINAL snapshot already exists     *)
(*    (crash between S4 and S5) recovery rolls forward: it commits that     *)
(*    generation (S5) without asking.                                      *)
(* S8 Snapshot-exit (`lnxctl snapshot-exit`): a guest snapshot does not end *)
(*    the run. It is captured like S4 into a generation with its own origin *)
(*    (SnapshotExit{run}), committed as latest WITHOUT changing the phase   *)
(*    (a dirty run stays dirty), and the run goes on acknowledging writes.  *)
(*    Recovery never rolls forward to it: only a final snapshot ends a run. *)
(*                                                                         *)
(* Disk and memory contents are "versions": version r is what run r wrote; *)
(* version Late is what the one snapshot-exit run wrote after its          *)
(* snapshot-exit. `parent` links each version to the one it started from,  *)
(* so "contains acknowledged write a" is ancestry. Generation slot r holds *)
(* run r's final snapshot (origin Snapshot{r}) or its salvage; slot        *)
(* ExitGen holds the one snapshot-exit generation (origin                  *)
(* SnapshotExit{exitRun}). At most one snapshot-exit happens per behavior. *)
(***************************************************************************)
EXTENDS Naturals, Integers, Sequences, FiniteSets

CONSTANTS MaxRuns, MaxFaults, AllowPowerLoss,
    \* "none", or one rule removed to show it is necessary:
    \*   "noDirtyPhase"   S3 skipped (phase stays running while commands run)
    \*   "noStageFsync"   S4 files and staging dir not fsynced
    \*   "noPauseFlush"   S4 captures the rootfs without the pmem flush
    \*   "gcBeforeCommit" S6 runs before the S5 commit
    \*   "exitLooksFinal" S8 recovery takes the snapshot-exit generation for
    \*                    the run's final snapshot and rolls forward to it
    \*   "advanceClears"  S8 committing the snapshot-exit generation resets
    \*                    the phase to running
    Mutation

NOMEM == -1
CORRUPT == -2
NoGen == -1
Runs == 1..MaxRuns
Late == MaxRuns + 1
Versions == 0..Late
ExitGen == MaxRuns + 1
Gens == 0..ExitGen
MaxObj == 3 * MaxRuns + 3

VARIABLES
    obj, nextObj,
    ns,     \* [rec |-> [latest, phase, run], gens |-> [Gens -> obj id], staging |-> obj id, runs |-> [Runs -> obj id]]
    hist,
    pc, run, parent, acked, dispatchedRun, faults,
    exitRun, \* the run that took the snapshot-exit (0: none yet)
    silentAckLoss, silentCorruptRestore, refused, gcDeletedLive

vars == <<obj, nextObj, ns, hist, pc, run, parent, acked, dispatchedRun, faults,
          exitRun, silentAckLoss, silentCorruptRestore, refused,
          gcDeletedLive>>

Ancestors(v) ==
    LET F[n \in 0..Late + 1] ==
          IF n = 0 THEN {v}
          ELSE LET prev == F[n - 1]
               IN prev \cup {parent[x] : x \in {y \in prev : y \in Versions /\ parent[y] \in Versions}}
    IN IF v \in Versions THEN F[Late + 1] ELSE {}
Contains(disk, a) == a \in Ancestors(disk)
AckedIn(disk) == \A a \in acked : Contains(disk, a)

MetaOp(n) == /\ ns' = n /\ hist' = Append(hist, n)
Commit(n) == /\ ns' = n /\ hist' = <<n>>

Rec(l, p, r) == [latest |-> l, phase |-> p, run |-> r]
LatestObj == ns.gens[ns.rec.latest]
Valid(o) == obj[o].mem # CORRUPT /\ obj[o].disk \in Versions
NoObj == [mem |-> NOMEM, memDur |-> NOMEM, disk |-> NOMEM, diskDur |-> NOMEM]

Init ==
    /\ obj = [i \in 1..MaxObj |-> IF i = 1 THEN [mem |-> 0, memDur |-> 0, disk |-> 0, diskDur |-> 0]
                                           ELSE NoObj]
    /\ nextObj = 2
    /\ ns = [rec |-> Rec(0, "stopped", 0),
             gens |-> [g \in Gens |-> IF g = 0 THEN 1 ELSE 0],
             staging |-> 0,
             runs |-> [r \in Runs |-> 0]]
    /\ hist = <<ns>>
    /\ pc = "recover"
    /\ run = 1
    /\ parent = [v \in Versions |-> -1]
    /\ acked = {}
    /\ dispatchedRun = FALSE
    /\ faults = 0
    /\ exitRun = 0
    /\ silentAckLoss = FALSE
    /\ silentCorruptRestore = FALSE
    /\ refused = FALSE
    /\ gcDeletedLive = FALSE

Alloc(rec) == /\ nextObj <= MaxObj
              /\ obj' = [obj EXCEPT ![nextObj] = rec]
              /\ nextObj' = nextObj + 1

Step(from, to) == pc = from /\ pc' = to

\* S6: remove every run dir, the staging dir and unreferenced generations.
GcNs(n) == [n EXCEPT !.runs = [r \in Runs |-> 0], !.staging = 0,
                     !.gens = [g \in Gens |-> IF g = n.rec.latest THEN n.gens[g] ELSE 0]]

-----------------------------------------------------------------------------
\* S7/S8: the generation recovery may roll the crashed run forward to: its
\* final snapshot (S4 done, S5 not), never its snapshot-exit generation.
FinalGenOf(r) ==
    IF r \in Runs /\ ns.gens[r] # 0 THEN r
    ELSE IF Mutation = "exitLooksFinal" /\ exitRun = r /\ ns.gens[ExitGen] # 0 THEN ExitGen
    ELSE NoGen
RollForwardPossible ==
    /\ ns.rec.phase \in {"running", "dirty"}
    /\ FinalGenOf(ns.rec.run) # NoGen
RollForward ==
    /\ pc = "recover"
    /\ RollForwardPossible
    /\ Commit([ns EXCEPT !.rec = Rec(FinalGenOf(ns.rec.run), "stopped", 0)])
    /\ UNCHANGED <<obj, nextObj, pc, run, parent, acked, dispatchedRun, faults, exitRun,
                   silentAckLoss, silentCorruptRestore, refused, gcDeletedLive>>

\* S7 recovery.
Recover ==
    /\ pc = "recover"
    /\ run <= MaxRuns
    /\ ~RollForwardPossible
    /\ IF ns.rec.phase = "dirty"
         THEN /\ pc' = "wedged"
              /\ UNCHANGED <<ns, hist, parent, silentAckLoss, silentCorruptRestore,
                             refused, gcDeletedLive>>
         ELSE LET n1 == GcNs(ns)
                  n2 == [n1 EXCEPT !.rec = Rec(ns.rec.latest, "stopped", 0)]
                  src == ns.gens[ns.rec.latest]
              IN /\ IF ns.rec.phase = "running" THEN Commit(n2) ELSE MetaOp(n1)
                 /\ gcDeletedLive' = (gcDeletedLive \/ src = 0)
                 \* manifest checksums: a torn file is always detected
                 /\ IF src = 0 \/ ~Valid(src)
                      THEN /\ pc' = "refused" /\ refused' = TRUE
                           /\ UNCHANGED <<parent, silentAckLoss, silentCorruptRestore>>
                      ELSE /\ pc' = "mkRun"
                           /\ parent' = [parent EXCEPT ![run] = obj[src].disk]
                           /\ silentAckLoss' = (silentAckLoss \/ ~AckedIn(obj[src].disk))
                           /\ UNCHANGED <<refused, silentCorruptRestore>>
    /\ UNCHANGED <<obj, nextObj, run, acked, dispatchedRun, faults, exitRun>>

\* S2: clone latest into runs/<run>/, fsync files and dirs.
MkRun ==
    /\ Step("mkRun", "walRunning")
    /\ Alloc(obj[LatestObj])
    /\ Commit([ns EXCEPT !.runs[run] = nextObj])
    /\ dispatchedRun' = FALSE
    /\ UNCHANGED <<run, parent, acked, faults, exitRun, silentAckLoss,
                   silentCorruptRestore, refused, gcDeletedLive>>

WalRunning ==
    /\ Step("walRunning", "dispatch")
    /\ Commit([ns EXCEPT !.rec = Rec(ns.rec.latest, "running", run)])
    /\ UNCHANGED <<obj, nextObj, run, parent, acked, dispatchedRun, faults, exitRun,
                   silentAckLoss, silentCorruptRestore, refused,
                   gcDeletedLive>>

\* S3: before the first command, phase := dirty (committed). Or no
\* command arrives at all during this run.
Dispatch ==
    /\ Step("dispatch", "run")
    /\ \E d \in BOOLEAN :
         /\ dispatchedRun' = d
         /\ IF d /\ Mutation # "noDirtyPhase"
              THEN Commit([ns EXCEPT !.rec = Rec(ns.rec.latest, "dirty", run)])
              ELSE UNCHANGED <<ns, hist>>
    /\ UNCHANGED <<obj, nextObj, run, parent, acked, faults, exitRun, silentAckLoss,
                   silentCorruptRestore, refused, gcDeletedLive>>

Live == ns.runs[run]

\* The guest writes version v; a flush (msync+fsync) of a dispatched
\* command's write acknowledges it.
GuestWrite(v) ==
    \E flush \in BOOLEAN :
      /\ obj' = [obj EXCEPT ![Live] = [@ EXCEPT !.disk = v,
                                               !.diskDur = IF flush THEN v ELSE @]]
      /\ acked' = IF flush /\ dispatchedRun THEN acked \cup {v} ELSE acked
      /\ IF flush THEN Commit(ns) ELSE UNCHANGED <<ns, hist>>

GuestRun ==
    /\ Step("run", "pause")
    /\ GuestWrite(run)
    /\ UNCHANGED <<nextObj, run, parent, dispatchedRun, faults, exitRun, silentAckLoss,
                   silentCorruptRestore, refused, gcDeletedLive>>

\* S4: pmem flush, then the capture of the paused VM into a staging dir with
\* fsynced files and dir. Memory holds the latest version the guest wrote.
Capture ==
    /\ nextObj <= MaxObj
    /\ LET flushed == Mutation # "noPauseFlush"
           liveDur == IF flushed THEN obj[Live].disk ELSE obj[Live].diskDur
           synced == Mutation # "noStageFsync"
           v == obj[Live].disk
       IN /\ obj' = [obj EXCEPT ![Live].diskDur = liveDur,
                                ![nextObj] = [mem |-> v,
                                              memDur |-> IF synced THEN v ELSE CORRUPT,
                                              disk |-> v,
                                              diskDur |-> liveDur]]
          /\ IF synced THEN Commit([ns EXCEPT !.staging = nextObj])
                       ELSE MetaOp([ns EXCEPT !.staging = nextObj])
    /\ nextObj' = nextObj + 1

\* S4: rename staging into generations/<g>, fsync generations/.
PublishTo(g) == Commit([ns EXCEPT !.gens[g] = ns.staging, !.staging = 0])

\* S4: the final snapshot.
Stage ==
    /\ Step("pause", "publishGen")
    /\ Capture
    /\ UNCHANGED <<run, parent, acked, dispatchedRun, faults, exitRun, silentAckLoss,
                   silentCorruptRestore, refused, gcDeletedLive>>

PublishGen ==
    /\ Step("publishGen", IF Mutation = "gcBeforeCommit" THEN "gc" ELSE "commitRec")
    /\ PublishTo(run)
    /\ UNCHANGED <<obj, nextObj, run, parent, acked, dispatchedRun, faults, exitRun,
                   silentAckLoss, silentCorruptRestore, refused,
                   gcDeletedLive>>

\* S5: the commit point.
CommitRec ==
    /\ Step("commitRec", IF Mutation = "gcBeforeCommit" THEN "recover" ELSE "gc")
    /\ Commit([ns EXCEPT !.rec = Rec(run, "stopped", 0)])
    /\ run' = IF Mutation = "gcBeforeCommit" THEN run + 1 ELSE run
    /\ UNCHANGED <<obj, nextObj, parent, acked, dispatchedRun, faults, exitRun,
                   silentAckLoss, silentCorruptRestore, refused,
                   gcDeletedLive>>

\* S6, no fsync.
Gc ==
    /\ Step("gc", IF Mutation = "gcBeforeCommit" THEN "commitRec" ELSE "recover")
    \* GC keeps only what the record references. (Keeping the just
    \* published generation as well would make early GC safe, because
    \* recovery rolls it forward; the rule is "GC = unreferenced at the time
    \* of the commit".)
    /\ LET n == GcNs(ns)
       IN /\ gcDeletedLive' = (gcDeletedLive \/ n.gens[ns.rec.latest] = 0)
          /\ MetaOp(n)
    /\ run' = IF Mutation = "gcBeforeCommit" THEN run ELSE run + 1
    /\ UNCHANGED <<obj, nextObj, parent, acked, dispatchedRun, faults, exitRun,
                   silentAckLoss, silentCorruptRestore, refused>>

-----------------------------------------------------------------------------
\* S8: `lnxctl snapshot-exit` (at most once per behavior). The guest asks for
\* a snapshot while its run goes on: capture (S4) into the snapshot-exit
\* generation, publish it, commit it as latest keeping the phase, resume.
ExitStage ==
    /\ Step("pause", "exitPublish")
    /\ exitRun = 0
    /\ Capture
    /\ exitRun' = run
    /\ UNCHANGED <<run, parent, acked, dispatchedRun, faults, silentAckLoss,
                   silentCorruptRestore, refused, gcDeletedLive>>

ExitPublish ==
    /\ Step("exitPublish", "exitCommit")
    /\ PublishTo(ExitGen)
    /\ UNCHANGED <<obj, nextObj, run, parent, acked, dispatchedRun, faults, exitRun,
                   silentAckLoss, silentCorruptRestore, refused,
                   gcDeletedLive>>

\* Store::advance_latest: latest := the snapshot-exit generation; the phase
\* (running or dirty, of this run) is kept.
ExitCommit ==
    /\ Step("exitCommit", "resumed")
    /\ Commit([ns EXCEPT !.rec = Rec(ExitGen,
                                     IF Mutation = "advanceClears" THEN "running"
                                                                    ELSE ns.rec.phase,
                                     run)])
    /\ UNCHANGED <<obj, nextObj, run, parent, acked, dispatchedRun, faults, exitRun,
                   silentAckLoss, silentCorruptRestore, refused,
                   gcDeletedLive>>

\* The resumed guest writes on top of what the snapshot-exit captured.
GuestRunLate ==
    /\ Step("resumed", "pause")
    /\ parent' = [parent EXCEPT ![Late] = obj[Live].disk]
    /\ GuestWrite(Late)
    /\ UNCHANGED <<nextObj, run, dispatchedRun, faults, exitRun, silentAckLoss,
                   silentCorruptRestore, refused, gcDeletedLive>>

-----------------------------------------------------------------------------
\* S7: `lnx recover --discard-run` (explicit consent to drop the run).
Discard ==
    /\ pc = "wedged"
    /\ Commit(GcNs([ns EXCEPT !.rec = Rec(ns.rec.latest, "stopped", 0)]))
    /\ acked' = {a \in acked : Contains(obj[LatestObj].disk, a)}
    /\ pc' = "recover"
    /\ UNCHANGED <<obj, nextObj, run, parent, dispatchedRun, faults, exitRun, silentAckLoss,
                   silentCorruptRestore, refused, gcDeletedLive>>

\* S7: `lnx recover --salvage`: stage runs/<r>/rootfs.ext4 as disk-only
\* generation r (S4), then commit it (S5).
SalvageStage ==
    /\ pc = "wedged"
    /\ LET r == ns.rec.run IN
       /\ ns.runs[r] # 0
       /\ Alloc([mem |-> NOMEM, memDur |-> NOMEM,
                 disk |-> obj[ns.runs[r]].disk, diskDur |-> obj[ns.runs[r]].diskDur])
       /\ Commit([ns EXCEPT !.staging = nextObj])
    /\ pc' = "salvagePublish"
    /\ UNCHANGED <<run, parent, acked, dispatchedRun, faults, exitRun, silentAckLoss,
                   silentCorruptRestore, refused, gcDeletedLive>>
SalvagePublish ==
    /\ Step("salvagePublish", "salvageCommit")
    /\ PublishTo(ns.rec.run)
    /\ UNCHANGED <<obj, nextObj, run, parent, acked, dispatchedRun, faults, exitRun,
                   silentAckLoss, silentCorruptRestore, refused,
                   gcDeletedLive>>
SalvageCommit ==
    /\ Step("salvageCommit", "recover")
    /\ Commit([ns EXCEPT !.rec = Rec(ns.rec.run, "stopped", 0)])
    /\ UNCHANGED <<obj, nextObj, run, parent, acked, dispatchedRun, faults, exitRun,
                   silentAckLoss, silentCorruptRestore, refused,
                   gcDeletedLive>>

-----------------------------------------------------------------------------
SalvagePCs == {"salvagePublish", "salvageCommit"}
RunPCs == {"mkRun", "walRunning", "dispatch", "run", "pause", "publishGen",
           "commitRec", "gc", "exitPublish", "exitCommit", "resumed"} \cup SalvagePCs

Kill ==
    /\ faults < MaxFaults
    /\ pc \in RunPCs
    /\ pc' = "recover"
    /\ run' = IF pc \in SalvagePCs THEN run ELSE run + 1
    /\ faults' = faults + 1
    /\ UNCHANGED <<obj, nextObj, ns, hist, parent, acked, dispatchedRun, exitRun,
                   silentAckLoss, silentCorruptRestore, refused,
                   gcDeletedLive>>

PowerLoss ==
    /\ AllowPowerLoss
    /\ faults < MaxFaults
    /\ pc \in RunPCs \cup {"recover", "wedged"}
    /\ \E k \in 1..Len(hist) : ns' = hist[k] /\ hist' = <<hist[k]>>
    /\ obj' = [i \in 1..MaxObj |-> [obj[i] EXCEPT !.mem = obj[i].memDur, !.disk = obj[i].diskDur]]
    /\ pc' = "recover"
    /\ run' = IF pc \in RunPCs \ SalvagePCs THEN run + 1 ELSE run
    /\ faults' = faults + 1
    /\ UNCHANGED <<nextObj, parent, acked, dispatchedRun, exitRun, silentAckLoss,
                   silentCorruptRestore, refused, gcDeletedLive>>

Next ==
    \/ Recover \/ RollForward \/ MkRun \/ WalRunning \/ Dispatch \/ GuestRun \/ Stage
    \/ PublishGen \/ CommitRec \/ Gc
    \/ ExitStage \/ ExitPublish \/ ExitCommit \/ GuestRunLate
    \/ Discard \/ SalvageStage \/ SalvagePublish \/ SalvageCommit
    \/ Kill \/ PowerLoss

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------
(******************************** PROPERTIES *******************************)

Durable == hist[1]

\* The durable record always names exactly one generation, which exists
\* durably and whose files are all durable.
LatestAlwaysDurable ==
    LET d == Durable
        o == d.gens[d.rec.latest]
    IN /\ o # 0
       /\ obj[o].memDur # CORRUPT
       /\ obj[o].diskDur \in Versions
\* The same for the live namespace (what recovery sees after kill -9).
LatestAlwaysValid ==
    LET o == ns.gens[ns.rec.latest] IN o # 0 /\ Valid(o)
\* Memory and disk of every published generation belong together.
GenerationsCoherent ==
    \A g \in Gens : ns.gens[g] # 0 => obj[ns.gens[g]].mem \in {obj[ns.gens[g]].disk, NOMEM}
NoRefusal == ~refused
NoSilentCorruptRestore == ~silentCorruptRestore
\* Every restore (cold or roll-forward) contains every acknowledged write,
\* including writes acknowledged after a snapshot-exit.
NoSilentAckLoss == ~silentAckLoss
GcNeverDeletesLatest == ~gcDeletedLive
\* A dirty run's work dir survives until the user decides.
DirtyRunPreserved ==
    (pc = "wedged") => (ns.rec.run \in Runs /\ ns.runs[ns.rec.run] # 0
                        /\ AckedIn(obj[ns.runs[ns.rec.run]].diskDur))

\* The user is only asked when the crashed run had not yet published its
\* final snapshot.
NoWedgeWhenCommittable == pc = "wedged" => ns.gens[ns.rec.run] = 0

\* Non-vacuity witnesses (expected to be violated).
WitnessSalvage == ~(pc = "recover" /\ ns.rec.latest # 0 /\ obj[ns.gens[ns.rec.latest]].mem = NOMEM)
WitnessCommit == ~(ns.rec.latest = MaxRuns)
WitnessWedged == pc # "wedged"
\* A run took a snapshot-exit, acknowledged a later write, and then
\* committed its final snapshot.
WitnessSnapshotExit ==
    ~(exitRun # 0 /\ ns.rec.latest = exitRun /\ ns.rec.phase = "stopped" /\ Late \in acked)

=============================================================================
