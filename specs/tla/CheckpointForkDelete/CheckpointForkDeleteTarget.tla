---------------------- MODULE CheckpointForkDeleteTarget ----------------------
(***************************************************************************)
(* TARGET design for checkpoints on top of the generation store of        *)
(* SnapshotCommitTarget (rules C1-C6 in specs/tla/README.md):              *)
(*                                                                         *)
(* C1 A checkpoint is a ref file checkpoints/<name>.ref containing a       *)
(*    generation id; it is created with tmp, fsync, rename, fsync dir,     *)
(*    only after generations/<gen>/ is complete and durable (S4).          *)
(*    `lnx checkpoint` reports success only after the ref rename.          *)
(* C2 `checkpoints list` reads only *.ref files whose generation exists.   *)
(* C3 `checkpoints delete` unlinks the ref (atomic). It does not touch the *)
(*    generation and needs no instance lock.                               *)
(* C4 fork resolves the ref, then pins the generation with flock(LOCK_SH)  *)
(*    on generations/<gen>/.pin and re-checks that generations/<gen> is    *)
(*    still in place (not renamed to trash); otherwise it fails with       *)
(*    "checkpoint deleted". It copies only while pinned; the pin dies with *)
(*    the process.                                                         *)
(* C5 GC (under the instance lock, see SnapshotCommitTarget S6) deletes a  *)
(*    generation only if no ref and no state record names it AND          *)
(*    flock(LOCK_EX|LOCK_NB) on its .pin succeeds; it then renames the dir *)
(*    to trash (atomic) before recursive deletion.                         *)
(* C6 Checkpoint capture happens under the instance lock (the owner, or a  *)
(*    foreground run), so GC never sees a staged-but-unreferenced          *)
(*    generation of an in-progress checkpoint; a crash leaves only garbage *)
(*    that the next GC removes.                                            *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS MaxCrashes, Gens,
    \* "none", or "noPin" (C4 without the flock pin) or "noRecheck" (C4
    \* pins without re-checking that the generation is still in place)
    Mutation

VARIABLES
    gens,       \* generations present at their final path (complete, immutable)
    staged,     \* generations being written (invisible)
    ref,        \* checkpoint ref -> gen, or 0
    pins,       \* gen -> number of live forks pinning it
    lockHolder, \* instance lock: "none" | "creator" | "gc"
    cpc, cgen,  \* creator
    fpc, fgen,  \* forker
    dpc,        \* deleter
    gcpc, gcgen,
    crashes,
    forkBad, createdIncomplete, listedIncomplete, gcDeletedLive

vars == <<gens, staged, ref, pins, lockHolder, cpc, cgen, fpc, fgen, dpc, gcpc,
          gcgen, crashes, forkBad, createdIncomplete, listedIncomplete, gcDeletedLive>>

Init ==
    /\ gens = {}
    /\ staged = {}
    /\ ref = 0
    /\ pins = [g \in Gens |-> 0]
    /\ lockHolder = "none"
    /\ cpc = "start" /\ cgen = 0
    /\ fpc = "idle" /\ fgen = 0
    /\ dpc = "idle"
    /\ gcpc = "idle" /\ gcgen = 0
    /\ crashes = 0
    /\ forkBad = FALSE
    /\ createdIncomplete = FALSE
    /\ listedIncomplete = FALSE
    /\ gcDeletedLive = FALSE

CanCrash == crashes < MaxCrashes

-----------------------------------------------------------------------------
(* Creator: take the instance lock, stage, publish the generation, write  *)
(* the ref (C1), release the lock. A second checkpoint reuses the flow.    *)
CLock ==
    /\ cpc = "start" /\ lockHolder = "none"
    /\ \E g \in Gens \ (gens \cup staged) : cgen' = g
    /\ lockHolder' = "creator"
    /\ cpc' = "stage"
    /\ UNCHANGED <<gens, staged, ref, pins, fpc, fgen, dpc, gcpc, gcgen, crashes,
                   forkBad, createdIncomplete, listedIncomplete, gcDeletedLive>>
CStage ==
    /\ cpc = "stage"
    /\ staged' = staged \cup {cgen}
    /\ cpc' = "publishGen"
    /\ UNCHANGED <<gens, ref, pins, lockHolder, cgen, fpc, fgen, dpc, gcpc, gcgen,
                   crashes, forkBad, createdIncomplete, listedIncomplete, gcDeletedLive>>
CPublishGen ==
    /\ cpc = "publishGen"
    /\ gens' = gens \cup {cgen}
    /\ staged' = staged \ {cgen}
    /\ cpc' = "ref"
    /\ UNCHANGED <<ref, pins, lockHolder, cgen, fpc, fgen, dpc, gcpc, gcgen,
                   crashes, forkBad, createdIncomplete, listedIncomplete, gcDeletedLive>>
CRef ==
    /\ cpc = "ref"
    /\ ref' = cgen
    /\ createdIncomplete' = (createdIncomplete \/ cgen \notin gens)
    /\ lockHolder' = "none"
    /\ cpc' = "done"
    /\ UNCHANGED <<gens, staged, pins, cgen, fpc, fgen, dpc, gcpc, gcgen, crashes,
                   forkBad, listedIncomplete, gcDeletedLive>>
CCrash ==
    /\ CanCrash
    /\ cpc \in {"stage", "publishGen", "ref"}
    /\ cpc' = "crashed"
    /\ lockHolder' = "none"      \* flock released by the kernel
    /\ crashes' = crashes + 1
    /\ UNCHANGED <<gens, staged, ref, pins, cgen, fpc, fgen, dpc, gcpc, gcgen,
                   forkBad, createdIncomplete, listedIncomplete, gcDeletedLive>>

-----------------------------------------------------------------------------
\* C2
List ==
    /\ ref # 0
    /\ listedIncomplete' = (listedIncomplete \/ ref \notin gens)
    /\ UNCHANGED <<gens, staged, ref, pins, lockHolder, cpc, cgen, fpc, fgen, dpc,
                   gcpc, gcgen, crashes, forkBad, createdIncomplete, gcDeletedLive>>

-----------------------------------------------------------------------------
\* C4 fork.
FResolve ==
    /\ fpc = "idle" /\ ref # 0
    /\ fgen' = ref
    /\ fpc' = "pin"
    /\ UNCHANGED <<gens, staged, ref, pins, lockHolder, cpc, cgen, dpc, gcpc, gcgen,
                   crashes, forkBad, createdIncomplete, listedIncomplete, gcDeletedLive>>
FPin ==
    /\ fpc = "pin"
    /\ IF fgen \in gens \/ Mutation = "noRecheck"
         THEN /\ pins' = IF Mutation = "noPin" THEN pins ELSE [pins EXCEPT ![fgen] = @ + 1]
              /\ fpc' = "copy"
         ELSE /\ fpc' = "failed"     \* "checkpoint deleted"
              /\ UNCHANGED pins
    /\ UNCHANGED <<gens, staged, ref, lockHolder, cpc, cgen, fgen, dpc, gcpc, gcgen,
                   crashes, forkBad, createdIncomplete, listedIncomplete, gcDeletedLive>>
\* Copy rootfs, memory files, stamps, host-share-state, manifest: one step
\* per group, each reading the immutable generation.
FCopy ==
    /\ fpc \in {"copy", "copy2", "copy3"}
    /\ forkBad' = (forkBad \/ fgen \notin gens)
    /\ fpc' = CASE fpc = "copy" -> "copy2" [] fpc = "copy2" -> "copy3" [] OTHER -> "publish"
    /\ UNCHANGED <<gens, staged, ref, pins, lockHolder, cpc, cgen, fgen, dpc, gcpc,
                   gcgen, crashes, createdIncomplete, listedIncomplete, gcDeletedLive>>
FPublish ==
    /\ fpc = "publish"
    /\ pins' = IF Mutation = "noPin" THEN pins ELSE [pins EXCEPT ![fgen] = @ - 1]
    /\ fpc' = "done"
    /\ UNCHANGED <<gens, staged, ref, lockHolder, cpc, cgen, fgen, dpc, gcpc, gcgen,
                   crashes, forkBad, createdIncomplete, listedIncomplete, gcDeletedLive>>
FCrash ==
    /\ CanCrash
    /\ fpc \in {"copy", "copy2", "copy3", "publish"}
    /\ pins' = IF Mutation = "noPin" THEN pins ELSE [pins EXCEPT ![fgen] = @ - 1]
    /\ fpc' = "crashed"
    /\ crashes' = crashes + 1
    /\ UNCHANGED <<gens, staged, ref, lockHolder, cpc, cgen, fgen, dpc, gcpc, gcgen,
                   forkBad, createdIncomplete, listedIncomplete, gcDeletedLive>>

-----------------------------------------------------------------------------
\* C3 delete.
Delete ==
    /\ dpc = "idle" /\ ref # 0
    /\ ref' = 0
    /\ dpc' = "done"
    /\ UNCHANGED <<gens, staged, pins, lockHolder, cpc, cgen, fpc, fgen, gcpc, gcgen,
                   crashes, forkBad, createdIncomplete, listedIncomplete, gcDeletedLive>>

-----------------------------------------------------------------------------
\* C5 GC: under the instance lock; staged leftovers of a crashed creator
\* are garbage too.
GcLock ==
    /\ gcpc = "idle" /\ lockHolder = "none"
    /\ lockHolder' = "gc"
    /\ gcpc' = "scan"
    /\ UNCHANGED <<gens, staged, ref, pins, cpc, cgen, fpc, fgen, dpc, gcgen, crashes,
                   forkBad, createdIncomplete, listedIncomplete, gcDeletedLive>>
GcTrash ==
    /\ gcpc = "scan"
    /\ \E g \in gens :
         /\ g # ref
         /\ pins[g] = 0          \* LOCK_EX|LOCK_NB on .pin succeeded
         /\ gens' = gens \ {g}   \* rename to trash, then rm -r
         /\ gcDeletedLive' = (gcDeletedLive \/ g = ref \/ pins[g] > 0)
    /\ UNCHANGED <<staged, ref, pins, lockHolder, cpc, cgen, fpc, fgen, dpc, gcpc, gcgen,
                   crashes, forkBad, createdIncomplete, listedIncomplete>>
GcStaged ==
    /\ gcpc = "scan" /\ staged # {}
    /\ staged' = {}
    /\ UNCHANGED <<gens, ref, pins, lockHolder, cpc, cgen, fpc, fgen, dpc, gcpc, gcgen,
                   crashes, forkBad, createdIncomplete, listedIncomplete, gcDeletedLive>>
GcDone ==
    /\ gcpc = "scan"
    /\ lockHolder' = "none"
    /\ gcpc' = "idle"
    /\ UNCHANGED <<gens, staged, ref, pins, cpc, cgen, fpc, fgen, dpc, gcgen, crashes,
                   forkBad, createdIncomplete, listedIncomplete, gcDeletedLive>>

\* A second checkpoint creation after the first one finished.
CAgain ==
    /\ cpc \in {"done", "crashed"}
    /\ \E g \in Gens \ (gens \cup staged \cup {cgen}) : TRUE
    /\ cpc' = "start"
    /\ UNCHANGED <<gens, staged, ref, pins, lockHolder, cgen, fpc, fgen, dpc, gcpc,
                   gcgen, crashes, forkBad, createdIncomplete, listedIncomplete,
                   gcDeletedLive>>

Next ==
    \/ CLock \/ CStage \/ CPublishGen \/ CRef \/ CCrash \/ CAgain
    \/ List
    \/ FResolve \/ FPin \/ FCopy \/ FPublish \/ FCrash
    \/ Delete
    \/ GcLock \/ GcTrash \/ GcStaged \/ GcDone

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------
ListShowsOnlyComplete == ~listedIncomplete
ForkIsComplete == ~forkBad
CreateSuccessImpliesComplete == ~createdIncomplete
GcNeverDeletesLiveGeneration == ~gcDeletedLive
\* A ref never names a missing generation.
RefIntegrity == ref # 0 => ref \in gens

\* Non-vacuity witnesses (expected to be violated).
WitnessForkCompletes == fpc # "done"
WitnessForkSeesDelete == fpc # "failed"
WitnessGcCollects == ~(ref = 0 /\ dpc = "done" /\ gens = {})

=============================================================================
