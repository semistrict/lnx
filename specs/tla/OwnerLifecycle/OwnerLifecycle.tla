---------------------------- MODULE OwnerLifecycle ----------------------------
(***************************************************************************)
(* CURRENT design of the lnx VM-owner lifecycle (main @ 077f0b09).         *)
(*                                                                         *)
(* Models CLI invocations (clients), the detached `_vm-owner` processes    *)
(* they spawn, the pid-file directory locks `owner-start.lock.d` and       *)
(* `bootstrap.lock.d`, the broker socket, the idle timer, draining, the    *)
(* final-snapshot outcome marker and the restore-work marker, a `stop`     *)
(* caller, kill -9 of any process at any step, and pid reuse.              *)
(*                                                                         *)
(* All file:line references are to main @ 077f0b09.                       *)
(*                                                                         *)
(* Granularity: every step that the code performs while holding the        *)
(* lock-dir guard flock (locks.rs:120-140) is one atomic action; crash     *)
(* points inside such a guard are modelled as separate "Crash...Mid"       *)
(* actions because flock does not undo half-done filesystem work.          *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
    NClients,     \* number of concurrent `lnx <cmd>` invocations
    MaxCrashes,   \* bound on kill -9 events (any process)
    MaxReuse,     \* bound on pid-reuse events
    WithStop      \* include one `stop`/`delete` caller (server.rs:1669, cli.rs:1390)

Clients == 1..NClients
\* Owner i is the `_vm-owner` process spawned by client i (runner.rs:267,
\* spawn_owner_process runner.rs:3219). Owner and client pids are distinct.
Owners == Clients
NoPid == 0   \* "pid of an owner that ran before the model started" (dead, never reused)

\* Lock-directory contents: absent, present without a pid file, or holding a pid.
LFree == [st |-> "free", pid |-> NoPid]
LNoPid == [st |-> "nopid", pid |-> NoPid]
Held(p) == [st |-> "held", pid |-> p]

ClientPCs == {"start", "tryExisting", "startLock", "spawn", "await",
              "open", "session", "finish", "done", "crashed"}
OwnerPCs  == {"idle", "acqBoot", "prep", "net", "restorePrep", "bind", "mark",
              "pending", "vmStart", "agent", "serving", "snap", "clearMarker",
              "outcome", "release", "failSock", "failRelease", "exited"}
\* Phases in which an owner owns bootstrap.lock.d.
HoldingPCs == {"prep", "net", "restorePrep", "bind", "mark", "pending", "vmStart",
               "agent", "serving", "snap", "clearMarker", "outcome", "release",
               "failSock", "failRelease"}

VARIABLES
    cpc, cres, conn,               \* clients
    opc, oexit, restoring, vmUp,   \* owners
    calive, oalive, cforeign, oforeign,
    startLock, bootLock, sockFile,
    outcome, marker, work, latest, unsaved,
    stopping, termReq, chan, cmd, cmdOwner, execCount,
    spc, signaledForeign, orphanAtSnapshot, execOnDraining,
    crashes, reuses

vars == <<cpc, cres, conn, opc, oexit, restoring, vmUp, calive, oalive,
          cforeign, oforeign, startLock, bootLock, sockFile, outcome, marker,
          work, latest, unsaved, stopping, termReq, chan, cmd, cmdOwner,
          execCount, spc, signaledForeign, orphanAtSnapshot, execOnDraining,
          crashes, reuses>>

-----------------------------------------------------------------------------
(* Process liveness as observed through kill(pid, 0).                      *)
(* process_alive (locks.rs:383-391) treats EPERM as alive, so a pid reused *)
(* by any unrelated process -- even another user's -- reads as alive.      *)
PidAliveC(i) == calive[i] \/ cforeign[i]
PidAliveO(i) == IF i = NoPid THEN FALSE ELSE oalive[i] \/ oforeign[i]

(* owner-start.lock.d: owner_start_lock_is_stale (locks.rs:367-381). A    *)
(* dir without starter.pid is stale after 10s; we let that time pass.      *)
StartLockFree ==
    \/ startLock.st \in {"free", "nopid"}
    \/ startLock.st = "held" /\ ~PidAliveC(startLock.pid)

(* bootstrap.lock.d: bootstrap_lock_is_stale (locks.rs:326-348).          *)
BootLockHeldLive == bootLock.st = "held" /\ PidAliveO(bootLock.pid)

(* validate_recovery_state_locked (runner.rs:299-333), including           *)
(* refuse_active_restore_work (snapshots.rs:272-291) and                   *)
(* validate_final_snapshot_outcome (snapshots.rs:504-536).                 *)
(* "wedged" = every command fails with "... snapshots clear ..." until the *)
(* user runs the destructive `lnx snapshots clear` (cli.rs:2646).          *)
RecoveryCheck ==
    IF BootLockHeldLive THEN "live"                                   \* 306-308
    ELSE IF marker /\ work THEN "wedged"                              \* 318
    ELSE IF bootLock.st = "nopid" THEN "wedged"                       \* 319-326
    ELSE IF bootLock.st = "held" /\ outcome.st = "none" THEN "wedged" \* snapshots.rs:514
    ELSE IF bootLock.st = "held" /\ outcome.st # "none"
            /\ outcome.pid # bootLock.pid THEN "wedged"               \* snapshots.rs:518
    ELSE IF outcome.st = "pending" THEN "wedged"                      \* snapshots.rs:523
    ELSE IF outcome.st = "error" THEN "wedged"                        \* snapshots.rs:528
    ELSE "ok"

(* connect_broker (runner.rs:1932-1972) completes the hello only while the *)
(* owner's accept loop (runner.rs:2794-3028) is running.                   *)
Connectable ==
    /\ sockFile \in Owners
    /\ oalive[sockFile]
    /\ opc[sockFile] = "serving"

(* Idle-timer accounting: a reservation is taken at accept time            *)
(* (runner.rs:2821) and a channel counts until the guest closes it.        *)
Active(i) ==
    \/ \E c \in Clients : cpc[c] = "open" /\ conn[c] = i
    \/ \E c \in Clients : chan[c] = "open" /\ conn[c] = i

CanCrash == crashes < MaxCrashes

-----------------------------------------------------------------------------
Init ==
    /\ cpc = [c \in Clients |-> "start"]
    /\ cres = [c \in Clients |-> "none"]
    /\ conn = [c \in Clients |-> NoPid]
    /\ opc = [i \in Owners |-> "idle"]
    /\ oexit = [i \in Owners |-> "none"]
    /\ restoring = [i \in Owners |-> FALSE]
    /\ vmUp = [i \in Owners |-> FALSE]
    /\ calive = [c \in Clients |-> TRUE]
    /\ oalive = [i \in Owners |-> FALSE]
    /\ cforeign = [c \in Clients |-> FALSE]
    /\ oforeign = [i \in Owners |-> FALSE]
    /\ startLock = LFree
    /\ bootLock = LFree
    /\ sockFile = NoPid
    \* Either a fresh instance or one whose previous owner exited cleanly.
    /\ latest \in BOOLEAN
    /\ outcome = IF latest THEN [st |-> "success", pid |-> NoPid]
                           ELSE [st |-> "none", pid |-> NoPid]
    /\ marker = FALSE
    /\ work = latest   \* .restore-work is left behind by every restored run
    /\ unsaved = FALSE
    /\ stopping = [i \in Owners |-> FALSE]
    /\ termReq = [i \in Owners |-> FALSE]
    /\ chan = [c \in Clients |-> "none"]
    /\ cmd = [c \in Clients |-> "none"]
    /\ cmdOwner = [c \in Clients |-> NoPid]
    /\ execCount = [c \in Clients |-> 0]
    /\ spc = IF WithStop THEN "idle" ELSE "done"
    /\ signaledForeign = FALSE
    /\ orphanAtSnapshot = FALSE
    /\ execOnDraining = FALSE
    /\ crashes = 0
    /\ reuses = 0

-----------------------------------------------------------------------------
(*************************** CLIENT: runner::run *************************)

\* runner.rs:175 validate_restore_work_for_command, under the guard flock.
CStart(c) ==
    /\ calive[c] /\ cpc[c] = "start"
    /\ IF RecoveryCheck = "wedged"
          THEN cpc' = [cpc EXCEPT ![c] = "done"] /\ cres' = [cres EXCEPT ![c] = "wedged"]
          ELSE cpc' = [cpc EXCEPT ![c] = "tryExisting"] /\ UNCHANGED cres
    /\ UNCHANGED <<conn, opc, oexit, restoring, vmUp, calive, oalive, cforeign,
                   oforeign, startLock, bootLock, sockFile, outcome, marker, work,
                   latest, unsaved, stopping, termReq, chan, cmd, cmdOwner,
                   execCount, spc, signaledForeign, orphanAtSnapshot,
                   execOnDraining, crashes, reuses>>

\* runner.rs:217-237 run_existing_broker_client.
CTryExisting(c) ==
    /\ calive[c] /\ cpc[c] = "tryExisting"
    /\ IF Connectable
          THEN cpc' = [cpc EXCEPT ![c] = "open"] /\ conn' = [conn EXCEPT ![c] = sockFile]
          ELSE cpc' = [cpc EXCEPT ![c] = "startLock"] /\ UNCHANGED conn
    /\ UNCHANGED <<cres, opc, oexit, restoring, vmUp, calive, oalive, cforeign,
                   oforeign, startLock, bootLock, sockFile, outcome, marker, work,
                   latest, unsaved, stopping, termReq, chan, cmd, cmdOwner,
                   execCount, spc, signaledForeign, orphanAtSnapshot,
                   execOnDraining, crashes, reuses>>

\* runner.rs:249, acquire_owner_start_or_run_client runner.rs:1831-1883,
\* OwnerStartLock::try_acquire locks.rs:261-275 (stale reclaim under guard).
CAcquireStart(c) ==
    /\ calive[c] /\ cpc[c] = "startLock"
    /\ StartLockFree
    /\ startLock' = Held(c)
    /\ cpc' = [cpc EXCEPT ![c] = "spawn"]
    /\ UNCHANGED <<cres, conn, opc, oexit, restoring, vmUp, calive, oalive,
                   cforeign, oforeign, bootLock, sockFile, outcome, marker, work,
                   latest, unsaved, stopping, termReq, chan, cmd, cmdOwner,
                   execCount, spc, signaledForeign, orphanAtSnapshot,
                   execOnDraining, crashes, reuses>>

\* runner.rs:1860-1872: start lock busy, an existing broker answers.
CStartLockAttach(c) ==
    /\ calive[c] /\ cpc[c] = "startLock"
    /\ ~StartLockFree
    /\ Connectable
    /\ cpc' = [cpc EXCEPT ![c] = "open"]
    /\ conn' = [conn EXCEPT ![c] = sockFile]
    /\ UNCHANGED <<cres, opc, oexit, restoring, vmUp, calive, oalive, cforeign,
                   oforeign, startLock, bootLock, sockFile, outcome, marker, work,
                   latest, unsaved, stopping, termReq, chan, cmd, cmdOwner,
                   execCount, spc, signaledForeign, orphanAtSnapshot,
                   execOnDraining, crashes, reuses>>

\* runner.rs:267 spawn_owner_process (process_group(0): survives the client).
CSpawn(c) ==
    /\ calive[c] /\ cpc[c] = "spawn"
    /\ opc' = [opc EXCEPT ![c] = "acqBoot"]
    /\ oalive' = [oalive EXCEPT ![c] = TRUE]
    /\ cpc' = [cpc EXCEPT ![c] = "await"]
    /\ UNCHANGED <<cres, conn, oexit, restoring, vmUp, calive, cforeign,
                   oforeign, startLock, bootLock, sockFile, outcome, marker, work,
                   latest, unsaved, stopping, termReq, chan, cmd, cmdOwner,
                   execCount, spc, signaledForeign, orphanAtSnapshot,
                   execOnDraining, crashes, reuses>>

\* run_broker_client_awaiting_owner runner.rs:3302-3313: connect succeeds.
CAwaitConnect(c) ==
    /\ calive[c] /\ cpc[c] = "await"
    /\ Connectable
    /\ cpc' = [cpc EXCEPT ![c] = "open"]
    /\ conn' = [conn EXCEPT ![c] = sockFile]
    /\ UNCHANGED <<cres, opc, oexit, restoring, vmUp, calive, oalive, cforeign,
                   oforeign, startLock, bootLock, sockFile, outcome, marker, work,
                   latest, unsaved, stopping, termReq, chan, cmd, cmdOwner,
                   execCount, spc, signaledForeign, orphanAtSnapshot,
                   execOnDraining, crashes, reuses>>

\* runner.rs:3321-3338: connect failed and the spawned owner has exited:
\* "lnx VM owner exited with <status> before the broker came up".
CAwaitOwnerExited(c) ==
    /\ calive[c] /\ cpc[c] = "await"
    /\ ~Connectable
    /\ opc[c] = "exited"
    /\ cres' = [cres EXCEPT ![c] =
                  CASE oexit[c] = "existing" -> "ownerExitedExisting"
                    [] oexit[c] = "ok"       -> "ownerExitedIdle"
                    [] oexit[c] = "crash"    -> "ownerCrashed"
                    [] oexit[c] = "wedged"   -> "wedged"
                    [] OTHER                 -> "ownerBootFailed"]
    /\ cpc' = [cpc EXCEPT ![c] = "finish"]
    /\ UNCHANGED <<conn, opc, oexit, restoring, vmUp, calive, oalive, cforeign,
                   oforeign, startLock, bootLock, sockFile, outcome, marker, work,
                   latest, unsaved, stopping, termReq, chan, cmd, cmdOwner,
                   execCount, spc, signaledForeign, orphanAtSnapshot,
                   execOnDraining, crashes, reuses>>

\* handle_broker_client runner.rs:3468-3588: the first message (OpenExec) is
\* checked against `stopping` and, under the clients mutex, registered and
\* forwarded to the guest agent in one critical section (3547-3575).
COpen(c) ==
    /\ calive[c] /\ cpc[c] = "open"
    /\ LET o == conn[c] IN
       IF ~oalive[o]
         THEN /\ cres' = [cres EXCEPT ![c] = "died"]
              /\ cpc' = [cpc EXCEPT ![c] = "finish"]
              /\ UNCHANGED <<chan, cmd, cmdOwner, execOnDraining>>
       ELSE IF stopping[o]
         THEN \* "VM owner is stopping after a final snapshot" (3469-3486)
              /\ cres' = [cres EXCEPT ![c] = "stopping"]
              /\ cpc' = [cpc EXCEPT ![c] = "finish"]
              /\ UNCHANGED <<chan, cmd, cmdOwner, execOnDraining>>
       ELSE /\ chan' = [chan EXCEPT ![c] = "open"]
            /\ cmd' = [cmd EXCEPT ![c] = "dispatched"]
            /\ cmdOwner' = [cmdOwner EXCEPT ![c] = o]
            /\ execOnDraining' = (execOnDraining \/ stopping[o])
            /\ cpc' = [cpc EXCEPT ![c] = "session"]
            /\ UNCHANGED cres
    /\ UNCHANGED <<conn, opc, oexit, restoring, vmUp, calive, oalive, cforeign,
                   oforeign, startLock, bootLock, sockFile, outcome, marker, work,
                   latest, unsaved, stopping, termReq, execCount, spc,
                   signaledForeign, orphanAtSnapshot, crashes, reuses>>

\* run_broker_session runner.rs:2137-2174: ExitStatus or Error ends it.
CSessionDone(c) ==
    /\ calive[c] /\ cpc[c] = "session"
    /\ chan[c] # "open"
    /\ cres' = [cres EXCEPT ![c] = chan[c]]   \* "ok" | "stopping" | "died"
    /\ cpc' = [cpc EXCEPT ![c] = "finish"]
    /\ UNCHANGED <<conn, opc, oexit, restoring, vmUp, calive, oalive, cforeign,
                   oforeign, startLock, bootLock, sockFile, outcome, marker, work,
                   latest, unsaved, stopping, termReq, chan, cmd, cmdOwner,
                   execCount, spc, signaledForeign, orphanAtSnapshot,
                   execOnDraining, crashes, reuses>>

\* runner.rs:284 drop(start_lock) (or scope exit on error), release_lock_dir
\* locks.rs:283-301 under the guard.
CFinish(c) ==
    /\ calive[c] /\ cpc[c] = "finish"
    /\ startLock' = IF startLock = Held(c) THEN LFree ELSE startLock
    /\ cpc' = [cpc EXCEPT ![c] = "done"]
    /\ UNCHANGED <<cres, conn, opc, oexit, restoring, vmUp, calive, oalive,
                   cforeign, oforeign, bootLock, sockFile, outcome, marker, work,
                   latest, unsaved, stopping, termReq, chan, cmd, cmdOwner,
                   execCount, spc, signaledForeign, orphanAtSnapshot,
                   execOnDraining, crashes, reuses>>

CrashClient(c) ==
    /\ CanCrash
    /\ calive[c] /\ cpc[c] \notin {"done", "crashed"}
    /\ calive' = [calive EXCEPT ![c] = FALSE]
    /\ cpc' = [cpc EXCEPT ![c] = "crashed"]
    /\ crashes' = crashes + 1
    /\ UNCHANGED <<cres, conn, opc, oexit, restoring, vmUp, oalive, cforeign,
                   oforeign, startLock, bootLock, sockFile, outcome, marker, work,
                   latest, unsaved, stopping, termReq, chan, cmd, cmdOwner,
                   execCount, spc, signaledForeign, orphanAtSnapshot,
                   execOnDraining, reuses>>

-----------------------------------------------------------------------------
(************************** OWNER: runner::run_owner ***********************)

OwnerUnchanged == UNCHANGED <<cpc, cres, conn, calive, cforeign, oforeign,
                              startLock, termReq, spc, signaledForeign, reuses>>

\* acquire_bootstrap_for_owner runner.rs:3353-3388 ->
\* try_acquire_validated_bootstrap runner.rs:335-348 -> try_acquire_lock_dir
\* locks.rs:169-191 (stale check, validate, remove stale dir, create, lease).
OAcqBootFree(i) ==
    /\ oalive[i] /\ opc[i] = "acqBoot"
    /\ ~BootLockHeldLive
    /\ IF RecoveryCheck = "wedged"
          THEN \* validate() bails -> run_owner returns Err, nothing to release
               /\ opc' = [opc EXCEPT ![i] = "exited"]
               /\ oexit' = [oexit EXCEPT ![i] = "wedged"]
               /\ oalive' = [oalive EXCEPT ![i] = FALSE]
               /\ UNCHANGED bootLock
          ELSE /\ bootLock' = Held(i)
               /\ opc' = [opc EXCEPT ![i] = "prep"]
               /\ UNCHANGED <<oexit, oalive>>
    /\ UNCHANGED <<restoring, vmUp, sockFile, outcome, marker, work, latest,
                   unsaved, stopping, chan, cmd, cmdOwner, execCount,
                   orphanAtSnapshot, execOnDraining, crashes>>
    /\ OwnerUnchanged

\* kill -9 between fs::create_dir and write_owner_lease (locks.rs:184-185):
\* the guard flock is released by the kernel, the pid-less dir stays.
OCrashMidAcquire(i) ==
    /\ CanCrash
    /\ oalive[i] /\ opc[i] = "acqBoot"
    /\ ~BootLockHeldLive
    /\ RecoveryCheck # "wedged"
    /\ bootLock' = LNoPid
    /\ opc' = [opc EXCEPT ![i] = "exited"]
    /\ oexit' = [oexit EXCEPT ![i] = "crash"]
    /\ oalive' = [oalive EXCEPT ![i] = FALSE]
    /\ crashes' = crashes + 1
    /\ UNCHANGED <<restoring, vmUp, sockFile, outcome, marker, work, latest,
                   unsaved, stopping, chan, cmd, cmdOwner, execCount,
                   orphanAtSnapshot, execOnDraining>>
    /\ OwnerUnchanged

\* runner.rs:3376-3378: lock busy but a broker answers -> owner exits 0
\* ("owner.exit reason=existing_broker", runner.rs:546-548).
OAcqBootExisting(i) ==
    /\ oalive[i] /\ opc[i] = "acqBoot"
    /\ BootLockHeldLive
    /\ Connectable
    /\ opc' = [opc EXCEPT ![i] = "exited"]
    /\ oexit' = [oexit EXCEPT ![i] = "existing"]
    /\ oalive' = [oalive EXCEPT ![i] = FALSE]
    /\ UNCHANGED <<bootLock, restoring, vmUp, sockFile, outcome, marker, work,
                   latest, unsaved, stopping, chan, cmd, cmdOwner, execCount,
                   orphanAtSnapshot, execOnDraining, crashes>>
    /\ OwnerUnchanged

\* runner.rs:550-558: refresh latest, remove a stale broker.sock.
OPrep(i) ==
    /\ oalive[i] /\ opc[i] = "prep"
    /\ sockFile' = NoPid
    /\ opc' = [opc EXCEPT ![i] = "net"]
    /\ UNCHANGED <<oexit, oalive, bootLock, restoring, vmUp, outcome, marker,
                   work, latest, unsaved, stopping, chan, cmd, cmdOwner,
                   execCount, orphanAtSnapshot, execOnDraining, crashes>>
    /\ OwnerUnchanged

\* start_vm runner.rs:791-820: re-validation, initramfs, start_network
\* (embedded gvproxy, runner.rs:1696-1709). Environment may fail it; the
\* Err propagates out of run_owner (573) and Drop releases the lock.
ONet(i) ==
    /\ oalive[i] /\ opc[i] = "net"
    /\ \E ok \in BOOLEAN :
          opc' = [opc EXCEPT ![i] = IF ok THEN "restorePrep" ELSE "failRelease"]
    /\ UNCHANGED <<oexit, oalive, bootLock, restoring, vmUp, sockFile, outcome,
                   marker, work, latest, unsaved, stopping, chan, cmd, cmdOwner,
                   execCount, orphanAtSnapshot, execOnDraining, crashes>>
    /\ OwnerUnchanged

\* prepare_restore_for_start snapshots.rs:566-599: cleanup_snapshot_runtime_state
\* (644-660) then clone latest -> .restore-work when latest exists.
ORestorePrep(i) ==
    /\ oalive[i] /\ opc[i] = "restorePrep"
    /\ restoring' = [restoring EXCEPT ![i] = latest]
    /\ work' = latest
    /\ marker' = FALSE
    /\ opc' = [opc EXCEPT ![i] = "bind"]
    /\ UNCHANGED <<oexit, oalive, bootLock, vmUp, sockFile, outcome, latest,
                   unsaved, stopping, chan, cmd, cmdOwner, execCount,
                   orphanAtSnapshot, execOnDraining, crashes>>
    /\ OwnerUnchanged

\* runner.rs:1001-1004 bind listeners (broker.sock file now exists, but the
\* accept loop is not running, so hellos time out).
OBind(i) ==
    /\ oalive[i] /\ opc[i] = "bind"
    /\ sockFile' = i
    /\ opc' = [opc EXCEPT ![i] = "mark"]
    /\ UNCHANGED <<oexit, oalive, bootLock, restoring, vmUp, outcome, marker,
                   work, latest, unsaved, stopping, chan, cmd, cmdOwner,
                   execCount, orphanAtSnapshot, execOnDraining, crashes>>
    /\ OwnerUnchanged

\* runner.rs:1182-1196 mark_restore_work_active.
OMark(i) ==
    /\ oalive[i] /\ opc[i] = "mark"
    /\ marker' = (marker \/ restoring[i])
    /\ opc' = [opc EXCEPT ![i] = "pending"]
    /\ UNCHANGED <<oexit, oalive, bootLock, restoring, vmUp, sockFile, outcome,
                   work, latest, unsaved, stopping, chan, cmd, cmdOwner,
                   execCount, orphanAtSnapshot, execOnDraining, crashes>>
    /\ OwnerUnchanged

\* runner.rs:1197 write_final_snapshot_pending -- BEFORE vm.start().
OPending(i) ==
    /\ oalive[i] /\ opc[i] = "pending"
    /\ outcome' = [st |-> "pending", pid |-> i]
    /\ opc' = [opc EXCEPT ![i] = "vmStart"]
    /\ UNCHANGED <<oexit, oalive, bootLock, restoring, vmUp, sockFile, marker,
                   work, latest, unsaved, stopping, chan, cmd, cmdOwner,
                   execCount, orphanAtSnapshot, execOnDraining, crashes>>
    /\ OwnerUnchanged

\* runner.rs:1207-1221 vm.start() on a thread. Either the guest starts
\* running (and may change the live rootfs), or krun fails before any guest
\* code runs (error delivered via vm_error_rx to accept_agent_hello).
OVmStart(i) ==
    /\ oalive[i] /\ opc[i] = "vmStart"
    /\ \E runs \in BOOLEAN :
          /\ vmUp' = [vmUp EXCEPT ![i] = runs]
          /\ unsaved' = (unsaved \/ runs)
          /\ opc' = [opc EXCEPT ![i] = IF runs THEN "agent" ELSE "failSock"]
    /\ UNCHANGED <<oexit, oalive, bootLock, restoring, sockFile, outcome, marker,
                   work, latest, stopping, chan, cmd, cmdOwner, execCount,
                   orphanAtSnapshot, execOnDraining, crashes>>
    /\ OwnerUnchanged

\* run_broker_owner runner.rs:2496-2583 agent hello / restore sync; may time
\* out (environment).
OAgent(i) ==
    /\ oalive[i] /\ opc[i] = "agent"
    /\ \E ok \in BOOLEAN : opc' = [opc EXCEPT ![i] = IF ok THEN "serving" ELSE "failSock"]
    /\ UNCHANGED <<oexit, oalive, bootLock, restoring, vmUp, sockFile, outcome,
                   marker, work, latest, unsaved, stopping, chan, cmd, cmdOwner,
                   execCount, orphanAtSnapshot, execOnDraining, crashes>>
    /\ OwnerUnchanged

\* runner.rs:1252-1263 cleanup_runtime_sockets on run_broker_owner Err.
OFailSock(i) ==
    /\ oalive[i] /\ opc[i] = "failSock"
    /\ sockFile' = IF sockFile = i THEN NoPid ELSE sockFile
    /\ vmUp' = [vmUp EXCEPT ![i] = FALSE]
    /\ opc' = [opc EXCEPT ![i] = "failRelease"]
    /\ UNCHANGED <<oexit, oalive, bootLock, restoring, outcome, marker, work,
                   latest, unsaved, stopping, chan, cmd, cmdOwner, execCount,
                   orphanAtSnapshot, execOnDraining, crashes>>
    /\ OwnerUnchanged

\* run_owner runner.rs:568-573 returns Err / exit(EXIT_RESTORE_FAILED);
\* BootstrapLock::drop (locks.rs:233-237) releases the dir. No outcome is
\* written on this path, whatever was written before stays.
OFailRelease(i) ==
    /\ oalive[i] /\ opc[i] = "failRelease"
    /\ bootLock' = IF bootLock = Held(i) THEN LFree ELSE bootLock
    /\ opc' = [opc EXCEPT ![i] = "exited"]
    /\ oexit' = [oexit EXCEPT ![i] = "bootFailed"]
    /\ oalive' = [oalive EXCEPT ![i] = FALSE]
    /\ UNCHANGED <<restoring, vmUp, sockFile, outcome, marker, work, latest,
                   unsaved, stopping, chan, cmd, cmdOwner, execCount,
                   orphanAtSnapshot, execOnDraining, crashes>>
    /\ OwnerUnchanged

\* Accept loop exit: idle TTL (runner.rs:3012-3023, starts_idle=true for the
\* daemon owner, runner.rs:560-563) or SIGTERM (2795-2813). Then the barrier
\* begin_broker_shutdown (1565-1595, 3032-3048): set stopping, send Error to
\* every registered channel WITHOUT stopping the guest command, remove the
\* socket, drop the listener.
ODrain(i) ==
    /\ oalive[i] /\ opc[i] = "serving"
    /\ (~Active(i) \/ termReq[i])
    /\ stopping' = [stopping EXCEPT ![i] = TRUE]
    /\ chan' = [c \in Clients |-> IF chan[c] = "open" /\ conn[c] = i
                                    THEN "stopping" ELSE chan[c]]
    /\ sockFile' = IF sockFile = i THEN NoPid ELSE sockFile
    /\ opc' = [opc EXCEPT ![i] = "snap"]
    /\ UNCHANGED <<oexit, oalive, bootLock, restoring, vmUp, outcome, marker,
                   work, latest, unsaved, cmd, cmdOwner, execCount,
                   orphanAtSnapshot, execOnDraining, crashes>>
    /\ OwnerUnchanged

\* Guest agent starts the dispatched command.
GuestExec(c) ==
    /\ cmd[c] = "dispatched"
    /\ oalive[cmdOwner[c]] /\ vmUp[cmdOwner[c]]
    /\ cmd' = [cmd EXCEPT ![c] = "running"]
    /\ execCount' = [execCount EXCEPT ![c] = @ + 1]
    /\ UNCHANGED <<cpc, cres, conn, opc, oexit, restoring, vmUp, calive, oalive,
                   cforeign, oforeign, startLock, bootLock, sockFile, outcome,
                   marker, work, latest, unsaved, stopping, termReq, chan,
                   cmdOwner, spc, signaledForeign, orphanAtSnapshot,
                   execOnDraining, crashes, reuses>>

\* Command exits; ExitStatus reaches the client if its channel is still open.
GuestExit(c) ==
    /\ cmd[c] = "running"
    /\ oalive[cmdOwner[c]] /\ vmUp[cmdOwner[c]]
    /\ opc[cmdOwner[c]] \in {"serving", "snap"}
    /\ cmd' = [cmd EXCEPT ![c] = "finished"]
    /\ chan' = [chan EXCEPT ![c] = IF @ = "open" THEN "ok" ELSE @]
    /\ UNCHANGED <<cpc, cres, conn, opc, oexit, restoring, vmUp, calive, oalive,
                   cforeign, oforeign, startLock, bootLock, sockFile, outcome,
                   marker, work, latest, unsaved, stopping, termReq, cmdOwner,
                   execCount, spc, signaledForeign, orphanAtSnapshot,
                   execOnDraining, crashes, reuses>>

\* serve_snapshot runner.rs:4462-4551 (guest sync + capture + publish +
\* promote; the internals are the SnapshotCommit spec). A command still
\* running in the guest is frozen into the snapshot and will resume, with
\* no client, after the next restore. Snapshot can fail (environment).
OSnap(i) ==
    /\ oalive[i] /\ opc[i] = "snap"
    /\ orphanAtSnapshot' = (orphanAtSnapshot \/
                            \E c \in Clients : cmdOwner[c] = i /\ cmd[c] \in {"dispatched", "running"})
    /\ \E ok \in BOOLEAN :
          /\ latest' = (latest \/ ok)
          /\ unsaved' = (unsaved /\ ~ok)
          /\ opc' = [opc EXCEPT ![i] = IF ok THEN "clearMarker" ELSE "outcome"]
    /\ cmd' = [c \in Clients |-> IF cmdOwner[c] = i /\ cmd[c] \in {"dispatched", "running"}
                                   THEN "frozen" ELSE cmd[c]]
    /\ vmUp' = [vmUp EXCEPT ![i] = FALSE]
    /\ UNCHANGED <<oexit, oalive, bootLock, restoring, sockFile, outcome, marker,
                   work, stopping, chan, cmdOwner, execCount, execOnDraining,
                   crashes>>
    /\ OwnerUnchanged

\* finish_restore_work_after_final_snapshot runner.rs:3095-3099,
\* snapshots.rs:310-320 (only on snapshot success).
OClearMarker(i) ==
    /\ oalive[i] /\ opc[i] = "clearMarker"
    /\ marker' = IF restoring[i] THEN FALSE ELSE marker
    /\ opc' = [opc EXCEPT ![i] = "outcome"]
    /\ UNCHANGED <<oexit, oalive, bootLock, restoring, vmUp, sockFile, outcome,
                   work, latest, unsaved, stopping, chan, cmd, cmdOwner,
                   execCount, orphanAtSnapshot, execOnDraining, crashes>>
    /\ OwnerUnchanged

\* runner.rs:580 write_final_snapshot_outcome (success iff snapshot ok).
OOutcome(i) ==
    /\ oalive[i] /\ opc[i] = "outcome"
    /\ outcome' = [st |-> IF unsaved THEN "error" ELSE "success", pid |-> i]
    /\ opc' = [opc EXCEPT ![i] = "release"]
    /\ UNCHANGED <<oexit, oalive, bootLock, restoring, vmUp, sockFile, marker,
                   work, latest, unsaved, stopping, chan, cmd, cmdOwner,
                   execCount, orphanAtSnapshot, execOnDraining, crashes>>
    /\ OwnerUnchanged

\* runner.rs:585 drop(bootstrap_lock): release_lock_dir locks.rs:283-301.
ORelease(i) ==
    /\ oalive[i] /\ opc[i] = "release"
    /\ bootLock' = IF bootLock = Held(i) THEN LFree ELSE bootLock
    /\ opc' = [opc EXCEPT ![i] = "exited"]
    /\ oexit' = [oexit EXCEPT ![i] = IF outcome.st = "success" THEN "ok" ELSE "bootFailed"]
    /\ oalive' = [oalive EXCEPT ![i] = FALSE]
    /\ UNCHANGED <<restoring, vmUp, sockFile, outcome, marker, work, latest,
                   unsaved, stopping, chan, cmd, cmdOwner, execCount,
                   orphanAtSnapshot, execOnDraining, crashes>>
    /\ OwnerUnchanged

\* kill -9 between removing owner.pid and remove_dir (locks.rs:295-298).
OCrashMidRelease(i) ==
    /\ CanCrash
    /\ oalive[i] /\ opc[i] \in {"release", "failRelease"}
    /\ bootLock = Held(i)
    /\ bootLock' = LNoPid
    /\ opc' = [opc EXCEPT ![i] = "exited"]
    /\ oexit' = [oexit EXCEPT ![i] = "crash"]
    /\ oalive' = [oalive EXCEPT ![i] = FALSE]
    /\ vmUp' = [vmUp EXCEPT ![i] = FALSE]
    /\ crashes' = crashes + 1
    /\ UNCHANGED <<restoring, sockFile, outcome, marker, work, latest, unsaved,
                   stopping, chan, cmd, cmdOwner, execCount, orphanAtSnapshot,
                   execOnDraining>>
    /\ OwnerUnchanged

\* kill -9 of an owner at any step (the VM dies with it). Lock dir,
\* broker.sock file, markers stay on disk.
CrashOwner(i) ==
    /\ CanCrash
    /\ oalive[i]
    /\ oalive' = [oalive EXCEPT ![i] = FALSE]
    /\ vmUp' = [vmUp EXCEPT ![i] = FALSE]
    /\ opc' = [opc EXCEPT ![i] = "exited"]
    /\ oexit' = [oexit EXCEPT ![i] = "crash"]
    /\ chan' = [c \in Clients |-> IF chan[c] = "open" /\ conn[c] = i THEN "died" ELSE chan[c]]
    /\ cmd' = [c \in Clients |-> IF cmdOwner[c] = i /\ cmd[c] \in {"dispatched", "running"}
                                   THEN "killed" ELSE cmd[c]]
    /\ crashes' = crashes + 1
    /\ UNCHANGED <<restoring, bootLock, sockFile, outcome, marker, work, latest,
                   unsaved, stopping, cmdOwner, execCount, orphanAtSnapshot,
                   execOnDraining>>
    /\ OwnerUnchanged

-----------------------------------------------------------------------------
(******************************* ENVIRONMENT ******************************)

\* The pid of a dead owner/client is reused by an unrelated process.
ReuseOwnerPid(i) ==
    /\ reuses < MaxReuse
    /\ opc[i] = "exited" /\ ~oalive[i] /\ ~oforeign[i]
    /\ oforeign' = [oforeign EXCEPT ![i] = TRUE]
    /\ reuses' = reuses + 1
    /\ UNCHANGED <<cpc, cres, conn, opc, oexit, restoring, vmUp, calive, oalive,
                   cforeign, startLock, bootLock, sockFile, outcome, marker, work,
                   latest, unsaved, stopping, termReq, chan, cmd, cmdOwner,
                   execCount, spc, signaledForeign, orphanAtSnapshot,
                   execOnDraining, crashes>>

ReuseClientPid(c) ==
    /\ reuses < MaxReuse
    /\ cpc[c] \in {"done", "crashed"} /\ ~calive[c] /\ ~cforeign[c]
    /\ cforeign' = [cforeign EXCEPT ![c] = TRUE]
    /\ reuses' = reuses + 1
    /\ UNCHANGED <<cpc, cres, conn, opc, oexit, restoring, vmUp, calive, oalive,
                   oforeign, startLock, bootLock, sockFile, outcome, marker, work,
                   latest, unsaved, stopping, termReq, chan, cmd, cmdOwner,
                   execCount, spc, signaledForeign, orphanAtSnapshot,
                   execOnDraining, crashes>>

\* Clients that finished normally are no longer alive processes.
ClientExit(c) ==
    /\ cpc[c] = "done" /\ calive[c]
    /\ calive' = [calive EXCEPT ![c] = FALSE]
    /\ UNCHANGED <<cpc, cres, conn, opc, oexit, restoring, vmUp, oalive,
                   cforeign, oforeign, startLock, bootLock, sockFile, outcome,
                   marker, work, latest, unsaved, stopping, termReq, chan, cmd,
                   cmdOwner, execCount, spc, signaledForeign, orphanAtSnapshot,
                   execOnDraining, crashes, reuses>>

\* `lnx delete` terminate_instance_owner (cli.rs:1390-1396) and the server's
\* stop_existing_instance (server.rs:1675-1680): read owner.pid, check
\* process_alive, then signal_process_group(pid, SIGTERM).
Stop ==
    /\ spc = "idle"
    /\ BootLockHeldLive
    /\ IF oalive[bootLock.pid]
          THEN /\ termReq' = [termReq EXCEPT ![bootLock.pid] = TRUE]
               /\ UNCHANGED signaledForeign
          ELSE /\ signaledForeign' = TRUE
               /\ UNCHANGED termReq
    /\ spc' = "done"
    /\ UNCHANGED <<cpc, cres, conn, opc, oexit, restoring, vmUp, calive, oalive,
                   cforeign, oforeign, startLock, bootLock, sockFile, outcome,
                   marker, work, latest, unsaved, stopping, chan, cmd, cmdOwner,
                   execCount, orphanAtSnapshot, execOnDraining, crashes, reuses>>

-----------------------------------------------------------------------------
Next ==
    \/ \E c \in Clients :
          \/ CStart(c) \/ CTryExisting(c) \/ CAcquireStart(c)
          \/ CStartLockAttach(c) \/ CSpawn(c) \/ CAwaitConnect(c)
          \/ CAwaitOwnerExited(c) \/ COpen(c) \/ CSessionDone(c) \/ CFinish(c)
          \/ CrashClient(c) \/ GuestExec(c) \/ GuestExit(c)
          \/ ReuseClientPid(c) \/ ClientExit(c)
    \/ \E i \in Owners :
          \/ OAcqBootFree(i) \/ OCrashMidAcquire(i) \/ OAcqBootExisting(i)
          \/ OPrep(i) \/ ONet(i) \/ ORestorePrep(i) \/ OBind(i) \/ OMark(i)
          \/ OPending(i) \/ OVmStart(i) \/ OAgent(i) \/ OFailSock(i)
          \/ OFailRelease(i) \/ ODrain(i) \/ OSnap(i) \/ OClearMarker(i)
          \/ OOutcome(i) \/ ORelease(i) \/ OCrashMidRelease(i)
          \/ CrashOwner(i) \/ ReuseOwnerPid(i)
    \/ Stop

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------
(******************************** PROPERTIES *******************************)

TypeOK ==
    /\ cpc \in [Clients -> ClientPCs]
    /\ opc \in [Owners -> OwnerPCs]
    /\ bootLock \in {LFree, LNoPid} \cup {Held(i) : i \in Owners}
    /\ startLock \in {LFree, LNoPid} \cup {Held(c) : c \in Clients}
    /\ sockFile \in {NoPid} \cup Owners

\* At most one live owner holds bootstrap.lock.d, at most one VM runs.
AtMostOneOwner ==
    Cardinality({i \in Owners : oalive[i] /\ opc[i] \in HoldingPCs}) <= 1
AtMostOneVm ==
    Cardinality({i \in Owners : vmUp[i]}) <= 1

\* A command is only dispatched to a guest whose owner is not draining.
NoExecOnDraining == ~execOnDraining

\* No command is executed twice.
NoDoubleExec == \A c \in Clients : execCount[c] <= 1

\* The instance demands the destructive `snapshots clear` only when there
\* really is guest state that a restart from `latest` would lose.
NoSpuriousWedge == RecoveryCheck = "wedged" => unsaved

\* The same property split by the artifact that causes the wedge, so each
\* root cause has its own reproducible counterexample.
NoOwnerAlive == \A i \in Owners : ~oalive[i]
\* (a) final-snapshot.outcome says "pending" for an owner whose VM never
\*     ran guest code.
NoWedgeFromPendingWithoutVm ==
    (NoOwnerAlive /\ RecoveryCheck = "wedged" /\ outcome.st = "pending") => unsaved
\* (b) bootstrap.lock.d names a dead owner that never wrote an outcome
\*     ("stopped without reporting" / "outcome for pid X, expected pid Y").
NoWedgeFromLeaseOutcomeMismatch ==
    (NoOwnerAlive /\ RecoveryCheck = "wedged" /\ bootLock.st = "held"
        /\ outcome.pid # bootLock.pid) => unsaved
\* (c) bootstrap.lock.d exists without owner.pid ("incomplete owner lease").
NoWedgeFromPidlessLease ==
    (NoOwnerAlive /\ bootLock.st = "nopid") => unsaved
\* (d) .restore-work.active left behind although the snapshot was published.
NoWedgeFromStaleRestoreMarker ==
    (NoOwnerAlive /\ marker /\ work) => unsaved

\* No lock can be held forever by a process that is not lnx. (While it is,
\* every client times out after 120s and `snapshots clear` refuses with
\* "has a running VM owner": no specific recovery is offered.)
NoStuckInstance ==
    /\ ~(bootLock.st = "held" /\ ~oalive[bootLock.pid] /\ oforeign[bootLock.pid])
    /\ ~(startLock.st = "held" /\ ~calive[startLock.pid] /\ cforeign[startLock.pid])

\* stop/delete never signal a process that is not the instance's owner.
NoSignalForeign == ~signaledForeign

\* Nothing is still executing in the guest when it is snapshotted.
NoOrphanAtSnapshot == ~orphanAtSnapshot

\* A client fails only for a reason that is real: its owner crashed, the
\* VM failed to boot, a stop was requested, or a (checked separately)
\* recovery condition. "owner exited 0 because another broker exists" and
\* "owner idled out before we connected" are spurious failures.
NoSpuriousClientFailure ==
    \A c \in Clients : cres[c] \notin {"ownerExitedExisting", "ownerExitedIdle"}
\* The two causes separately.
NoFailureFromIdleExitBeforeAttach == \A c \in Clients : cres[c] # "ownerExitedIdle"
NoFailureFromExistingBrokerExit == \A c \in Clients : cres[c] # "ownerExitedExisting"

=============================================================================
