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
(*    manifest.json with gen id, parent gen, run id, size+checksum of every *)
(*    file), runs/<run>/ (live work dir), generations/.staging-<gen>/.      *)
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
(*    own S5 commit. If generations/<run> of the crashed run already exists *)
(*    (crash between S4 and S5) recovery rolls forward: it commits that     *)
(*    generation (S5) without asking.                                      *)
(***************************************************************************)
EXTENDS Naturals, Integers, Sequences, FiniteSets

CONSTANTS MaxRuns, MaxFaults, AllowPowerLoss,
    \* "none", or one rule removed to show it is necessary:
    \*   "noDirtyPhase"   S3 skipped (phase stays running while commands run)
    \*   "noStageFsync"   S4 files and staging dir not fsynced
    \*   "noPauseFlush"   S4 captures the rootfs without the pmem flush
    \*   "gcBeforeCommit" S6 runs before the S5 commit
    Mutation

NOMEM == -1
CORRUPT == -2
Gens == 0..MaxRuns
Runs == 1..MaxRuns
MaxObj == 3 * MaxRuns + 2

VARIABLES
    obj, nextObj,
    ns,     \* [rec |-> [latest, phase, run], gens |-> [Gens -> obj id], staging |-> obj id, runs |-> [Runs -> obj id]]
    hist,
    pc, run, parent, acked, dispatchedRun, faults,
    silentAckLoss, silentCorruptRestore, refused, gcDeletedLive

vars == <<obj, nextObj, ns, hist, pc, run, parent, acked, dispatchedRun, faults,
          silentAckLoss, silentCorruptRestore, refused,
          gcDeletedLive>>

Ancestors(g) ==
    LET F[n \in 0..MaxRuns + 1] ==
          IF n = 0 THEN {g}
          ELSE LET prev == F[n - 1]
               IN prev \cup {parent[x] : x \in {y \in prev : y \in Gens /\ parent[y] \in Gens}}
    IN IF g \in Gens THEN F[MaxRuns + 1] ELSE {}
Contains(diskGen, a) == a \in Ancestors(diskGen)
AckedIn(diskGen) == \A a \in acked : Contains(diskGen, a)

MetaOp(n) == /\ ns' = n /\ hist' = Append(hist, n)
Commit(n) == /\ ns' = n /\ hist' = <<n>>

Rec(l, p, r) == [latest |-> l, phase |-> p, run |-> r]
LatestObj == ns.gens[ns.rec.latest]
Valid(o) == obj[o].mem # CORRUPT /\ obj[o].disk \in Gens
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
    /\ parent = [g \in Gens |-> -1]
    /\ acked = {}
    /\ dispatchedRun = FALSE
    /\ faults = 0
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
\* S7 roll-forward: the crashed run already published generations/<run>
\* (S4 done, S5 not): commit it instead of asking the user.
RollForwardPossible ==
    /\ ns.rec.phase \in {"running", "dirty"}
    /\ ns.rec.run \in Runs
    /\ ns.gens[ns.rec.run] # 0
RollForward ==
    /\ pc = "recover"
    /\ RollForwardPossible
    /\ Commit([ns EXCEPT !.rec = Rec(ns.rec.run, "stopped", 0)])
    /\ UNCHANGED <<obj, nextObj, pc, run, parent, acked, dispatchedRun, faults,
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
    /\ UNCHANGED <<obj, nextObj, run, acked, dispatchedRun, faults>>

\* S2: clone latest into runs/<run>/, fsync files and dirs.
MkRun ==
    /\ Step("mkRun", "walRunning")
    /\ Alloc(obj[LatestObj])
    /\ Commit([ns EXCEPT !.runs[run] = nextObj])
    /\ dispatchedRun' = FALSE
    /\ UNCHANGED <<run, parent, acked, faults, silentAckLoss,
                   silentCorruptRestore, refused, gcDeletedLive>>

WalRunning ==
    /\ Step("walRunning", "dispatch")
    /\ Commit([ns EXCEPT !.rec = Rec(ns.rec.latest, "running", run)])
    /\ UNCHANGED <<obj, nextObj, run, parent, acked, dispatchedRun, faults,
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
    /\ UNCHANGED <<obj, nextObj, run, parent, acked, faults, silentAckLoss,
                   silentCorruptRestore, refused, gcDeletedLive>>

Live == ns.runs[run]

\* The guest writes; a flush (msync+fsync) of a dispatched command's write
\* acknowledges it.
GuestRun ==
    /\ Step("run", "pause")
    /\ \E flush \in BOOLEAN :
         /\ obj' = [obj EXCEPT ![Live] = [@ EXCEPT !.disk = run,
                                                  !.diskDur = IF flush THEN run ELSE @]]
         /\ acked' = IF flush /\ dispatchedRun THEN acked \cup {run} ELSE acked
         /\ IF flush THEN Commit(ns) ELSE UNCHANGED <<ns, hist>>
    /\ UNCHANGED <<nextObj, run, parent, dispatchedRun, faults, silentAckLoss,
                   silentCorruptRestore, refused, gcDeletedLive>>

\* S4: pmem flush, then staging with fsynced files and dir.
Stage ==
    /\ Step("pause", "publishGen")
    /\ nextObj <= MaxObj
    /\ LET flushed == Mutation # "noPauseFlush"
           liveDur == IF flushed THEN obj[Live].disk ELSE obj[Live].diskDur
           synced == Mutation # "noStageFsync"
       IN /\ obj' = [obj EXCEPT ![Live].diskDur = liveDur,
                                ![nextObj] = [mem |-> run,
                                              memDur |-> IF synced THEN run ELSE CORRUPT,
                                              disk |-> obj[Live].disk,
                                              diskDur |-> liveDur]]
          /\ IF synced THEN Commit([ns EXCEPT !.staging = nextObj])
                       ELSE MetaOp([ns EXCEPT !.staging = nextObj])
    /\ nextObj' = nextObj + 1
    /\ UNCHANGED <<run, parent, acked, dispatchedRun, faults, silentAckLoss,
                   silentCorruptRestore, refused, gcDeletedLive>>

\* S4: rename staging into generations/, fsync generations/.
PublishGen ==
    /\ Step("publishGen", IF Mutation = "gcBeforeCommit" THEN "gc" ELSE "commitRec")
    /\ Commit([ns EXCEPT !.gens[run] = ns.staging, !.staging = 0])
    /\ UNCHANGED <<obj, nextObj, run, parent, acked, dispatchedRun, faults,
                   silentAckLoss, silentCorruptRestore, refused,
                   gcDeletedLive>>

\* S5: the commit point.
CommitRec ==
    /\ Step("commitRec", IF Mutation = "gcBeforeCommit" THEN "recover" ELSE "gc")
    /\ Commit([ns EXCEPT !.rec = Rec(run, "stopped", 0)])
    /\ run' = IF Mutation = "gcBeforeCommit" THEN run + 1 ELSE run
    /\ UNCHANGED <<obj, nextObj, parent, acked, dispatchedRun, faults,
                   silentAckLoss, silentCorruptRestore, refused,
                   gcDeletedLive>>

\* S6, no fsync.
Gc ==
    /\ Step("gc", IF Mutation = "gcBeforeCommit" THEN "commitRec" ELSE "recover")
    \* GC keeps only what the record references. (Keeping the just
    \* published generation as well would make early GC safe, because
    \* recovery rolls it forward; the rule is "GC = unreferenced at the time
    \* of the commit".)
    /\ LET n == [ns EXCEPT !.runs = [r \in Runs |-> 0], !.staging = 0,
                           !.gens = [g \in Gens |-> IF g = ns.rec.latest THEN ns.gens[g] ELSE 0]]
       IN /\ gcDeletedLive' = (gcDeletedLive \/ n.gens[ns.rec.latest] = 0)
          /\ MetaOp(n)
    /\ run' = IF Mutation = "gcBeforeCommit" THEN run ELSE run + 1
    /\ UNCHANGED <<obj, nextObj, parent, acked, dispatchedRun, faults, silentAckLoss,
                   silentCorruptRestore, refused>>

-----------------------------------------------------------------------------
\* S7: `lnx recover --discard-run` (explicit consent to drop the run).
Discard ==
    /\ pc = "wedged"
    /\ Commit(GcNs([ns EXCEPT !.rec = Rec(ns.rec.latest, "stopped", 0)]))
    /\ acked' = {a \in acked : Contains(obj[LatestObj].disk, a)}
    /\ pc' = "recover"
    /\ UNCHANGED <<obj, nextObj, run, parent, dispatchedRun, faults, silentAckLoss,
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
    /\ UNCHANGED <<run, parent, acked, dispatchedRun, faults, silentAckLoss,
                   silentCorruptRestore, refused, gcDeletedLive>>
SalvagePublish ==
    /\ Step("salvagePublish", "salvageCommit")
    /\ Commit([ns EXCEPT !.gens[ns.rec.run] = ns.staging, !.staging = 0])
    /\ UNCHANGED <<obj, nextObj, run, parent, acked, dispatchedRun, faults,
                   silentAckLoss, silentCorruptRestore, refused,
                   gcDeletedLive>>
SalvageCommit ==
    /\ Step("salvageCommit", "recover")
    /\ Commit([ns EXCEPT !.rec = Rec(ns.rec.run, "stopped", 0)])
    /\ UNCHANGED <<obj, nextObj, run, parent, acked, dispatchedRun, faults,
                   silentAckLoss, silentCorruptRestore, refused,
                   gcDeletedLive>>

-----------------------------------------------------------------------------
RunPCs == {"mkRun", "walRunning", "dispatch", "run", "pause", "publishGen",
           "commitRec", "gc", "salvagePublish", "salvageCommit"}

Kill ==
    /\ faults < MaxFaults
    /\ pc \in RunPCs
    /\ pc' = "recover"
    /\ run' = IF pc \in {"salvagePublish", "salvageCommit"} THEN run ELSE run + 1
    /\ faults' = faults + 1
    /\ UNCHANGED <<obj, nextObj, ns, hist, parent, acked, dispatchedRun,
                   silentAckLoss, silentCorruptRestore, refused,
                   gcDeletedLive>>

PowerLoss ==
    /\ AllowPowerLoss
    /\ faults < MaxFaults
    /\ pc \in RunPCs \cup {"recover", "wedged"}
    /\ \E k \in 1..Len(hist) : ns' = hist[k] /\ hist' = <<hist[k]>>
    /\ obj' = [i \in 1..MaxObj |-> [obj[i] EXCEPT !.mem = obj[i].memDur, !.disk = obj[i].diskDur]]
    /\ pc' = "recover"
    /\ run' = IF pc \in RunPCs \ {"salvagePublish", "salvageCommit"} THEN run + 1 ELSE run
    /\ faults' = faults + 1
    /\ UNCHANGED <<nextObj, parent, acked, dispatchedRun, silentAckLoss,
                   silentCorruptRestore, refused, gcDeletedLive>>

Next ==
    \/ Recover \/ RollForward \/ MkRun \/ WalRunning \/ Dispatch \/ GuestRun \/ Stage
    \/ PublishGen \/ CommitRec \/ Gc
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
       /\ obj[o].diskDur \in Gens
\* The same for the live namespace (what recovery sees after kill -9).
LatestAlwaysValid ==
    LET o == ns.gens[ns.rec.latest] IN o # 0 /\ Valid(o)
\* Memory and disk of every published generation belong together.
GenerationsCoherent ==
    \A g \in Gens : ns.gens[g] # 0 => obj[ns.gens[g]].mem \in {obj[ns.gens[g]].disk, NOMEM}
NoRefusal == ~refused
NoSilentCorruptRestore == ~silentCorruptRestore
NoSilentAckLoss == ~silentAckLoss
GcNeverDeletesLatest == ~gcDeletedLive
\* A dirty run's work dir survives until the user decides.
DirtyRunPreserved ==
    (pc = "wedged") => (ns.rec.run \in Runs /\ ns.runs[ns.rec.run] # 0
                        /\ AckedIn(obj[ns.runs[ns.rec.run]].diskDur))

\* The user is only asked when the crashed run had not yet published.
NoWedgeWhenCommittable == pc = "wedged" => ns.gens[ns.rec.run] = 0

\* Non-vacuity witnesses (expected to be violated).
WitnessSalvage == ~(pc = "recover" /\ ns.rec.latest # 0 /\ obj[ns.gens[ns.rec.latest]].mem = NOMEM)
WitnessCommit == ~(ns.rec.latest = MaxRuns)
WitnessWedged == pc # "wedged"

=============================================================================
