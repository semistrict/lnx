------------------------- MODULE CheckpointForkDelete -------------------------
(***************************************************************************)
(* CURRENT design (main @ 077f0b09) of `lnx checkpoint`, `checkpoints     *)
(* list`, `fork <checkpoint>` and `checkpoints delete`, running            *)
(* concurrently on one checkpoint id. File-level model: each file of a     *)
(* checkpoint directory is absent, "old" (seeded from latest, stale),      *)
(* "partial" (being written in place) or "new" (final content).            *)
(*                                                                         *)
(* Files: mem = vmstate.bin+pages.img, disk = rootfs.ext4, stamps =        *)
(* initramfs.stamp+launch.json+deterministic stamps, hss =                 *)
(* host-share-state/, manifest = snapshot.meta, meta = checkpoint.meta.    *)
(*                                                                         *)
(* Two creation paths (CreateMode):                                        *)
(*  "live"       a VM owner is running: the owner writes the checkpoint IN *)
(*               PLACE into checkpoints/<id>/ (runner.rs:2849-2930).       *)
(*  "foreground" no owner: run_foreground captures into the sibling        *)
(*               checkpoints/.<id>.next/ and renames it to checkpoints/<id>*)
(*               (capture_snapshot_for_publish runner.rs:4554-4586,        *)
(*               publish_snapshot_dir snapshots.rs:815-863), then releases *)
(*               the owner lock, then the CLI writes checkpoint.meta       *)
(*               (cli.rs:2589, checkpoints.rs:45-62).                       *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS CreateMode, WithFork, WithDelete, MaxCrashes,
    Precreated   \* the checkpoint already exists, complete, at the start

Files == {"mem", "disk", "stamps", "hss", "manifest", "meta"}
\* What validate_memory_checkpoint (checkpoints.rs:275-363) requires.
Validated == {"mem", "disk", "stamps"}
\* What a usable fork needs to carry over (clone_snapshot_dir
\* checkpoints.rs:488-511 + clone_host_share_state 513-518).
Needed == {"mem", "disk", "stamps", "hss", "manifest"}
Dirs == {"final", "temp"}    \* checkpoints/<id>/ and checkpoints/.<id>.next/

None == [f \in Files |-> "none"]

VARIABLES
    dir,        \* [Dirs -> BOOLEAN] directory exists
    files,      \* [Dirs -> [Files -> state]]
    cpc,        \* creator
    ownerLock,  \* owner lock held (blocks `checkpoints delete`, cli.rs:2624-2638)
    fpc, fsrc, fcopy,   \* forker: chosen dir, values copied so far
    dpc,        \* deleter
    crashes,
    forkBad, createdIncomplete, listedIncomplete

vars == <<dir, files, cpc, ownerLock, fpc, fsrc, fcopy, dpc, crashes,
          forkBad, createdIncomplete, listedIncomplete>>

Complete(d) == dir[d] /\ \A f \in Files : files[d][f] = "new"
\* checkpoints::list (checkpoints.rs:64-84) returns every directory entry
\* under checkpoints/, including dot-named temp dirs, with or without
\* checkpoint.meta.
Visible == {d \in Dirs : dir[d]}

Init ==
    /\ dir = [d \in Dirs |-> Precreated /\ d = "final"]
    /\ files = [d \in Dirs |-> IF Precreated /\ d = "final" THEN [f \in Files |-> "new"] ELSE None]
    /\ cpc = IF Precreated THEN "done" ELSE "start"
    /\ ownerLock = (CreateMode = "live" /\ ~Precreated)
    /\ fpc = IF WithFork THEN "idle" ELSE "done"
    /\ fsrc = "final"
    /\ fcopy = None
    /\ dpc = IF WithDelete THEN "idle" ELSE "done"
    /\ crashes = 0
    /\ forkBad = FALSE
    /\ createdIncomplete = FALSE
    /\ listedIncomplete = FALSE

-----------------------------------------------------------------------------
(******************************** CREATOR **********************************)

W == IF CreateMode = "live" THEN "final" ELSE "temp"   \* where capture writes

SetFile(d, f, v) == files' = [files EXCEPT ![d][f] = v]

CStep(from, to) == cpc = from /\ cpc' = to

\* seed_incremental_snapshot (runner.rs:4588-4627): create the dir and
\* clone latest's restore files into it (stale content, complete-looking).
CSeed ==
    /\ CStep("start", "memPartial")
    /\ dir' = [dir EXCEPT ![W] = TRUE]
    /\ files' = [files EXCEPT ![W] = [f \in Files |->
                    IF f \in {"mem", "disk", "stamps", "manifest"} THEN "old" ELSE "none"]]
    /\ UNCHANGED <<ownerLock, fpc, fsrc, fcopy, dpc, crashes, forkBad,
                   createdIncomplete, listedIncomplete>>

\* snapshot_with_file_copy: dirty pages and vmstate written in place.
CMemPartial ==
    /\ CStep("memPartial", "memDone") /\ SetFile(W, "mem", "partial")
    /\ UNCHANGED <<dir, ownerLock, fpc, fsrc, fcopy, dpc, crashes, forkBad,
                   createdIncomplete, listedIncomplete>>
CMemDone ==
    /\ CStep("memDone", "disk") /\ SetFile(W, "mem", "new")
    /\ UNCHANGED <<dir, ownerLock, fpc, fsrc, fcopy, dpc, crashes, forkBad,
                   createdIncomplete, listedIncomplete>>
\* ... and rootfs.ext4 cloned from the live disk.
CDisk ==
    /\ CStep("disk", "stamps") /\ SetFile(W, "disk", "new")
    /\ UNCHANGED <<dir, ownerLock, fpc, fsrc, fcopy, dpc, crashes, forkBad,
                   createdIncomplete, listedIncomplete>>
\* copy_snapshot_stamp runner.rs:4639-4679.
CStamps ==
    /\ CStep("stamps", "hssPartial") /\ SetFile(W, "stamps", "new")
    /\ UNCHANGED <<dir, ownerLock, fpc, fsrc, fcopy, dpc, crashes, forkBad,
                   createdIncomplete, listedIncomplete>>
\* copy_host_share_state_to_snapshot runner.rs:4717-4725 (recursive clone).
CHssPartial ==
    /\ CStep("hssPartial", "hssDone") /\ SetFile(W, "hss", "partial")
    /\ UNCHANGED <<dir, ownerLock, fpc, fsrc, fcopy, dpc, crashes, forkBad,
                   createdIncomplete, listedIncomplete>>
CHssDone ==
    /\ CStep("hssDone", "manifest") /\ SetFile(W, "hss", "new")
    /\ UNCHANGED <<dir, ownerLock, fpc, fsrc, fcopy, dpc, crashes, forkBad,
                   createdIncomplete, listedIncomplete>>
\* write_snapshot_lifecycle_manifest snapshots.rs:116-137.
CManifest ==
    /\ CStep("manifest", IF CreateMode = "live" THEN "meta" ELSE "publish")
    /\ SetFile(W, "manifest", "new")
    /\ UNCHANGED <<dir, ownerLock, fpc, fsrc, fcopy, dpc, crashes, forkBad,
                   createdIncomplete, listedIncomplete>>
\* Foreground only: publish_snapshot_dir renames .<id>.next -> <id>, then
\* run_foreground drops the bootstrap lock (runner.rs:694) before the CLI
\* writes checkpoint.meta.
CPublish ==
    /\ CStep("publish", "meta")
    /\ dir' = [dir EXCEPT !["final"] = TRUE, !["temp"] = FALSE]
    /\ files' = [files EXCEPT !["final"] = files["temp"], !["temp"] = None]
    /\ ownerLock' = FALSE
    /\ UNCHANGED <<fpc, fsrc, fcopy, dpc, crashes, forkBad, createdIncomplete,
                   listedIncomplete>>
\* checkpoints::write_metadata: create_dir_all + write checkpoint.meta, then
\* the CLI prints the checkpoint name (success).
CMeta ==
    /\ CStep("meta", "done")
    /\ dir' = [dir EXCEPT !["final"] = TRUE]
    /\ files' = [files EXCEPT !["final"]["meta"] = "new"]
    /\ createdIncomplete' = (createdIncomplete \/
                             \E f \in Files \ {"meta"} : files["final"][f] # "new")
    /\ UNCHANGED <<ownerLock, fpc, fsrc, fcopy, dpc, crashes, forkBad, listedIncomplete>>

\* A live owner eventually idles out and releases its lock.
OwnerExit ==
    /\ ownerLock /\ CreateMode = "live" /\ cpc \in {"meta", "done", "crashed"}
    /\ ownerLock' = FALSE
    /\ UNCHANGED <<dir, files, cpc, fpc, fsrc, fcopy, dpc, crashes, forkBad,
                   createdIncomplete, listedIncomplete>>

\* kill -9 of the creating process(es): nothing cleans up.
CCrash ==
    /\ crashes < MaxCrashes
    /\ cpc \notin {"start", "done", "crashed"}
    /\ cpc' = "crashed"
    /\ ownerLock' = FALSE
    /\ crashes' = crashes + 1
    /\ UNCHANGED <<dir, files, fpc, fsrc, fcopy, dpc, forkBad, createdIncomplete,
                   listedIncomplete>>

-----------------------------------------------------------------------------
(********************************* LISTER **********************************)

\* `lnx checkpoints list` (cli.rs:2605-2622) at any time.
List ==
    /\ Visible # {}
    /\ listedIncomplete' = (listedIncomplete \/ \E d \in Visible : ~Complete(d))
    /\ UNCHANGED <<dir, files, cpc, ownerLock, fpc, fsrc, fcopy, dpc, crashes,
                   forkBad, createdIncomplete>>

-----------------------------------------------------------------------------
(********************************* FORKER **********************************)
(* fork_checkpoint cli.rs:2799-2836 -> checkpoints::fork 113-200. No lock  *)
(* on the source checkpoint.                                               *)

FStep(from, to) == fpc = from /\ fpc' = to

\* resolve (by id; any listed dir) + validate_memory_checkpoint.
FResolve ==
    /\ fpc = "idle"
    /\ \E d \in Visible :
         /\ fsrc' = d
         /\ fpc' = IF \A f \in Validated : files[d][f] # "none" THEN "disk" ELSE "failed"
    /\ UNCHANGED <<dir, files, cpc, ownerLock, fcopy, dpc, crashes, forkBad,
                   createdIncomplete, listedIncomplete>>
\* clone_or_copy rootfs.ext4: fails if it vanished.
FDisk ==
    /\ fpc = "disk"
    /\ IF dir[fsrc] /\ files[fsrc]["disk"] # "none"
         THEN fpc' = "snap" /\ fcopy' = [fcopy EXCEPT !["disk"] = files[fsrc]["disk"]]
         ELSE fpc' = "failed" /\ UNCHANGED fcopy
    /\ UNCHANGED <<dir, files, cpc, ownerLock, fsrc, dpc, crashes, forkBad,
                   createdIncomplete, listedIncomplete>>
\* clone_snapshot_dir: a missing file is silently skipped (NotFound => {}).
FSnap ==
    /\ FStep("snap", "hss")
    /\ fcopy' = [f \in Files |-> IF f \in {"mem", "stamps", "manifest", "meta"}
                                   THEN (IF dir[fsrc] THEN files[fsrc][f] ELSE "none")
                                   ELSE fcopy[f]]
    /\ UNCHANGED <<dir, files, cpc, ownerLock, fsrc, dpc, crashes, forkBad,
                   createdIncomplete, listedIncomplete>>
\* clone_host_share_state: skipped if missing.
FHss ==
    /\ FStep("hss", "publish")
    /\ fcopy' = [fcopy EXCEPT !["hss"] = IF dir[fsrc] THEN files[fsrc]["hss"] ELSE "none"]
    /\ UNCHANGED <<dir, files, cpc, ownerLock, fsrc, dpc, crashes, forkBad,
                   createdIncomplete, listedIncomplete>>
\* rename staging -> instances/<dest>: the fork now exists.
FPublish ==
    /\ FStep("publish", "done")
    /\ forkBad' = (forkBad \/ \E f \in Needed : fcopy[f] # "new")
    /\ UNCHANGED <<dir, files, cpc, ownerLock, fsrc, fcopy, dpc, crashes,
                   createdIncomplete, listedIncomplete>>

-----------------------------------------------------------------------------
(********************************* DELETER *********************************)
(* delete_checkpoint cli.rs:2624-2638: with_validated_stopped_instance     *)
(* (refused while an owner holds the lock), resolve, remove_dir_all       *)
(* (checkpoints.rs:101-111), which unlinks entries one at a time.          *)

DStart ==
    /\ dpc = "idle"
    /\ ~ownerLock
    /\ dir["final"]
    /\ dpc' = "rm"
    /\ UNCHANGED <<dir, files, cpc, ownerLock, fpc, fsrc, fcopy, crashes, forkBad,
                   createdIncomplete, listedIncomplete>>
DRmFile ==
    /\ dpc = "rm"
    /\ \E f \in Files :
         /\ files["final"][f] # "none"
         /\ SetFile("final", f, "none")
    /\ UNCHANGED <<dir, cpc, ownerLock, fpc, fsrc, fcopy, dpc, crashes, forkBad,
                   createdIncomplete, listedIncomplete>>
DRmDir ==
    /\ dpc = "rm"
    /\ \A f \in Files : files["final"][f] = "none"
    /\ dir' = [dir EXCEPT !["final"] = FALSE]
    /\ dpc' = "done"
    /\ UNCHANGED <<files, cpc, ownerLock, fpc, fsrc, fcopy, crashes, forkBad,
                   createdIncomplete, listedIncomplete>>

Next ==
    \/ CSeed \/ CMemPartial \/ CMemDone \/ CDisk \/ CStamps \/ CHssPartial
    \/ CHssDone \/ CManifest \/ CPublish \/ CMeta \/ OwnerExit \/ CCrash
    \/ List
    \/ FResolve \/ FDisk \/ FSnap \/ FHss \/ FPublish
    \/ DStart \/ DRmFile \/ DRmDir

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------
\* `checkpoints list` only shows complete checkpoints.
ListShowsOnlyComplete == ~listedIncomplete
\* A published fork carries the checkpoint's final memory, disk, stamps,
\* host-share-state and manifest.
ForkIsComplete == ~forkBad
\* `lnx checkpoint` only reports success for a complete checkpoint.
CreateSuccessImpliesComplete == ~createdIncomplete

=============================================================================
