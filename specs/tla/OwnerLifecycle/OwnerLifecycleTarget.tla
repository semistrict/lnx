------------------------- MODULE OwnerLifecycleTarget -------------------------
(***************************************************************************)
(* TARGET design for the lnx VM-owner lifecycle. Same actors and the same  *)
(* environment (kill -9 of any process at any step) as OwnerLifecycle.tla, *)
(* with these rules (numbered as in specs/tla/README.md, "Owner rules"):   *)
(*                                                                         *)
(* O1 One lock: flock(LOCK_EX|LOCK_NB) on instance_dir/instance.lock. The  *)
(*    client takes it and the spawned owner inherits the locked open file  *)
(*    description; the client then closes its copy. The kernel releases it *)
(*    when the holder dies. No pid files are consulted for exclusion, so   *)
(*    pid reuse cannot affect it (no pid-reuse action is needed here).     *)
(* O2 One instance record, instance_dir/state, replaced atomically          *)
(*    (tmp, fsync, rename, fsync dir) and only by the lock holder:         *)
(*    {latest generation, phase, run_id}. phase is                         *)
(*      stopped  - latest is authoritative, no run in progress             *)
(*      running  - written immediately BEFORE vm.start() (write-ahead)      *)
(*      dirty    - written BEFORE the first command is dispatched           *)
(*    The final commit writes {latest := new generation, phase := stopped} *)
(*    in ONE record replacement. It replaces final-snapshot.outcome,       *)
(*    .restore-work.active, owner-start.lock.d and bootstrap.lock.d.        *)
(* O3 Recovery (by whoever next takes the lock): stopped -> nothing;       *)
(*    running (dead owner, nothing dispatched) -> delete runs/<run_id>,     *)
(*    write stopped, continue automatically; dirty -> typed error          *)
(*    CrashedWithUnsavedState; `lnx recover --discard-run` deletes only    *)
(*    runs/<run_id> and writes stopped (latest kept).                       *)
(* O4 Owner start failure (before serving) is cleaned up by the owner      *)
(*    itself: delete runs/<run_id>, write stopped, exit "boot failed".     *)
(* O5 Clients never fail on races: "owner is stopping" (idle drain) is a   *)
(*    typed Retry returned before the command is dispatched; a broker that *)
(*    vanished before dispatch is retried. After dispatch nothing is       *)
(*    retried (at-most-once).                                              *)
(* O6 Idle drain only with no channels, no accepted-but-unregistered       *)
(*    connection, and no pending spawn reservation (the spawning client    *)
(*    keeps a pipe to the owner; the reservation ends when that client     *)
(*    attaches or the pipe reaches EOF).                                   *)
(* O7 Stop drain (Stop RPC): new opens fail with Stopped; in-flight guest  *)
(*    commands are terminated in the guest and their exit status is        *)
(*    delivered; the snapshot is taken only when no command is in flight. *)
(* O8 Stop never signals by pid. It sends Stop over the owner's control    *)
(*    socket; a forced kill is allowed only for the verified lock holder   *)
(*    (pid + process start time recorded by the owner, checked while       *)
(*    LOCK_NB still fails), which this model treats as atomic.             *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS NClients, MaxCrashes, MaxSpawns, WithStop, WithRecover

Clients == 1..NClients
Owners == Clients    \* owner slot i is (re)used by client i's spawned owners
NoPid == 0

ClientPCs == {"start", "lock", "spawn", "wait", "open", "session", "done", "crashed"}
OwnerPCs == {"idle", "boot", "wal", "vmStart", "agent", "bootFail", "serving",
             "draining", "snap", "cleanup", "exited"}
HoldingPCs == OwnerPCs \ {"idle", "exited"}

NoLock == [who |-> "none", id |-> NoPid]
ClientLock(c) == [who |-> "client", id |-> c]
OwnerLock(i) == [who |-> "owner", id |-> i]

VARIABLES
    cpc, cres, conn, spawns, calive,
    opc, oexit, oalive, vmUp, spawnRes,
    lock, phase, phaseRun, latest, work,
    stopping, termReq, chan, cmd, cmdOwner, execCount,
    vmRan, dispatched,               \* ghost: guest ran / command dispatched since last commit
    spc, rpc,                        \* stop caller, user running `lnx recover`
    orphanAtSnapshot, execOnDraining, autoDiscardedAck, autoRecovered, wedgedWithoutAck,
    crashes

vars == <<cpc, cres, conn, spawns, calive, opc, oexit, oalive, vmUp, spawnRes,
          lock, phase, phaseRun, latest, work, stopping, termReq, chan, cmd,
          cmdOwner, execCount, vmRan, dispatched, spc, rpc, orphanAtSnapshot,
          execOnDraining, autoDiscardedAck, autoRecovered, wedgedWithoutAck, crashes>>

LockFree == lock = NoLock
ServingOwner == {i \in Owners : oalive[i] /\ opc[i] = "serving"}
Connectable == ServingOwner # {}
Active(i) ==
    \/ \E c \in Clients : cpc[c] = "open" /\ conn[c] = i
    \/ \E c \in Clients : chan[c] = "open" /\ conn[c] = i
InFlight(i) == \E c \in Clients : cmdOwner[c] = i /\ cmd[c] \in {"dispatched", "running"}
CanCrash == crashes < MaxCrashes
\* When an owner process exits, connections that were accepted but have
\* not sent OpenExec yet are reset (they can never reach a later owner).
BreakConns(i) == [c \in Clients |-> IF cpc[c] = "open" /\ conn[c] = i THEN NoPid ELSE conn[c]]

\* The instance is refusing work and needs `lnx recover`.
Wedged == LockFree /\ phase = "dirty"

Init ==
    /\ cpc = [c \in Clients |-> "start"]
    /\ cres = [c \in Clients |-> "none"]
    /\ conn = [c \in Clients |-> NoPid]
    /\ spawns = [c \in Clients |-> 0]
    /\ calive = [c \in Clients |-> TRUE]
    /\ opc = [i \in Owners |-> "idle"]
    /\ oexit = [i \in Owners |-> "none"]
    /\ oalive = [i \in Owners |-> FALSE]
    /\ vmUp = [i \in Owners |-> FALSE]
    /\ spawnRes = [i \in Owners |-> FALSE]
    /\ lock = NoLock
    /\ phase = "stopped"
    /\ phaseRun = NoPid
    /\ latest \in BOOLEAN
    /\ work = [i \in Owners |-> FALSE]
    /\ stopping = [i \in Owners |-> FALSE]
    /\ termReq = [i \in Owners |-> FALSE]
    /\ chan = [c \in Clients |-> "none"]
    /\ cmd = [c \in Clients |-> "none"]
    /\ cmdOwner = [c \in Clients |-> NoPid]
    /\ execCount = [c \in Clients |-> 0]
    /\ vmRan = FALSE
    /\ dispatched = FALSE
    /\ spc = IF WithStop THEN "idle" ELSE "done"
    /\ rpc = IF WithRecover THEN "idle" ELSE "done"
    /\ orphanAtSnapshot = FALSE
    /\ execOnDraining = FALSE
    /\ autoDiscardedAck = FALSE
    /\ autoRecovered = FALSE
    /\ wedgedWithoutAck = FALSE
    /\ crashes = 0

-----------------------------------------------------------------------------
(********************************* CLIENT *********************************)

TStart(c) ==
    /\ calive[c] /\ cpc[c] = "start"
    /\ IF Connectable
          THEN \E i \in ServingOwner :
                  /\ conn' = [conn EXCEPT ![c] = i]
                  /\ cpc' = [cpc EXCEPT ![c] = "open"]
          ELSE /\ cpc' = [cpc EXCEPT ![c] = "lock"]
               /\ UNCHANGED conn
    /\ UNCHANGED <<cres, spawns, calive, opc, oexit, oalive, vmUp, spawnRes, lock,
                   phase, phaseRun, latest, work, stopping, termReq, chan, cmd,
                   cmdOwner, execCount, vmRan, dispatched, spc, rpc,
                   orphanAtSnapshot, execOnDraining, autoDiscardedAck, autoRecovered,
                   wedgedWithoutAck, crashes>>

\* O1 + O3: flock(LOCK_NB) and recovery, atomically under the lock.
TLock(c) ==
    /\ calive[c] /\ cpc[c] = "lock"
    /\ IF ~LockFree
         THEN /\ cpc' = [cpc EXCEPT ![c] = "wait"]
              /\ UNCHANGED <<cres, lock, phase, phaseRun, work, vmRan,
                             autoDiscardedAck, autoRecovered, wedgedWithoutAck>>
       ELSE IF phase = "dirty"
         THEN \* CrashedWithUnsavedState; the lock is dropped again.
              /\ cres' = [cres EXCEPT ![c] = "crashedUnsaved"]
              /\ cpc' = [cpc EXCEPT ![c] = "done"]
              /\ wedgedWithoutAck' = (wedgedWithoutAck \/ ~dispatched)
              /\ UNCHANGED <<lock, phase, phaseRun, work, vmRan, autoDiscardedAck,
                             autoRecovered>>
       ELSE IF spawns[c] >= MaxSpawns
         THEN \* model bound only: stop exploring further spawns
              /\ cpc' = [cpc EXCEPT ![c] = "done"]
              /\ cres' = [cres EXCEPT ![c] = "bound"]
              /\ UNCHANGED <<lock, phase, phaseRun, work, vmRan, autoDiscardedAck,
                             autoRecovered, wedgedWithoutAck>>
         ELSE /\ lock' = ClientLock(c)
              \* phase = running: the dead owner dispatched nothing; drop its run.
              /\ autoDiscardedAck' = (autoDiscardedAck \/ (phase = "running" /\ dispatched))
              /\ autoRecovered' = (autoRecovered \/ phase = "running")
              /\ work' = IF phase = "running" THEN [work EXCEPT ![phaseRun] = FALSE] ELSE work
              /\ vmRan' = IF phase = "running" THEN FALSE ELSE vmRan
              /\ phase' = "stopped"
              /\ phaseRun' = NoPid
              /\ cpc' = [cpc EXCEPT ![c] = "spawn"]
              /\ UNCHANGED <<cres, wedgedWithoutAck>>
    /\ UNCHANGED <<conn, spawns, calive, opc, oexit, oalive, vmUp, spawnRes, latest,
                   stopping, termReq, chan, cmd, cmdOwner, execCount, dispatched,
                   spc, rpc, orphanAtSnapshot, execOnDraining, crashes>>

\* Spawn the owner; it inherits the locked file description (O1) and a
\* reservation pipe (O6).
TSpawn(c) ==
    /\ calive[c] /\ cpc[c] = "spawn"
    /\ ~oalive[c]
    /\ lock' = OwnerLock(c)
    /\ oalive' = [oalive EXCEPT ![c] = TRUE]
    /\ opc' = [opc EXCEPT ![c] = "boot"]
    /\ oexit' = [oexit EXCEPT ![c] = "none"]
    /\ stopping' = [stopping EXCEPT ![c] = FALSE]
    /\ termReq' = [termReq EXCEPT ![c] = FALSE]
    /\ spawnRes' = [spawnRes EXCEPT ![c] = TRUE]
    /\ spawns' = [spawns EXCEPT ![c] = @ + 1]
    /\ cpc' = [cpc EXCEPT ![c] = "wait"]
    /\ UNCHANGED <<cres, conn, calive, vmUp, phase, phaseRun, latest, work, chan,
                   cmd, cmdOwner, execCount, vmRan, dispatched, spc, rpc,
                   orphanAtSnapshot, execOnDraining, autoDiscardedAck, autoRecovered,
                   wedgedWithoutAck, crashes>>

\* Wait for a broker or for the lock to be released.
TWait(c) ==
    /\ calive[c] /\ cpc[c] = "wait"
    /\ \/ /\ Connectable
          /\ \E i \in ServingOwner :
                /\ conn' = [conn EXCEPT ![c] = i]
                /\ cpc' = [cpc EXCEPT ![c] = "open"]
          /\ UNCHANGED cres
       \/ /\ ~Connectable /\ LockFree
          /\ IF opc[c] = "exited" /\ oexit[c] = "bootFailed" /\ spawnRes[c]
                THEN \* our own owner reported a VM start failure: real error
                     /\ cres' = [cres EXCEPT ![c] = "bootFailed"]
                     /\ cpc' = [cpc EXCEPT ![c] = "done"]
                ELSE /\ cpc' = [cpc EXCEPT ![c] = "start"]   \* retry (O5)
                     /\ UNCHANGED cres
          /\ UNCHANGED conn
    /\ UNCHANGED <<spawns, calive, opc, oexit, oalive, vmUp, spawnRes, lock,
                   phase, phaseRun, latest, work, stopping, termReq, chan, cmd,
                   cmdOwner, execCount, vmRan, dispatched, spc, rpc,
                   orphanAtSnapshot, execOnDraining, autoDiscardedAck, autoRecovered,
                   wedgedWithoutAck, crashes>>

\* OpenExec. Under the owner's clients mutex: check stopping, write the
\* dirty phase record if needed (O2), register and dispatch.
TOpen(c) ==
    /\ calive[c] /\ cpc[c] = "open"
    /\ LET o == conn[c] IN
       IF o = NoPid
         THEN \* broker vanished before dispatch: retry (O5)
              /\ cpc' = [cpc EXCEPT ![c] = "start"]
              /\ UNCHANGED <<cres, chan, cmd, cmdOwner, phase, phaseRun, dispatched,
                             spawnRes, execOnDraining>>
       ELSE IF stopping[o]
         THEN /\ IF termReq[o]
                    THEN /\ cres' = [cres EXCEPT ![c] = "stopped"]
                         /\ cpc' = [cpc EXCEPT ![c] = "done"]
                    ELSE /\ cpc' = [cpc EXCEPT ![c] = "start"]   \* Retry (O5)
                         /\ UNCHANGED cres
              /\ UNCHANGED <<chan, cmd, cmdOwner, phase, phaseRun, dispatched,
                             spawnRes, execOnDraining>>
       ELSE /\ phase' = "dirty"
            /\ phaseRun' = o
            /\ chan' = [chan EXCEPT ![c] = "open"]
            /\ cmd' = [cmd EXCEPT ![c] = "dispatched"]
            /\ cmdOwner' = [cmdOwner EXCEPT ![c] = o]
            /\ dispatched' = TRUE
            /\ execOnDraining' = (execOnDraining \/ stopping[o])
            /\ spawnRes' = IF c = o THEN [spawnRes EXCEPT ![o] = FALSE] ELSE spawnRes
            /\ cpc' = [cpc EXCEPT ![c] = "session"]
            /\ UNCHANGED cres
    /\ UNCHANGED <<conn, spawns, calive, opc, oexit, oalive, vmUp, lock, latest,
                   work, stopping, termReq, execCount, vmRan, spc, rpc,
                   orphanAtSnapshot, autoDiscardedAck, autoRecovered, wedgedWithoutAck,
                   crashes>>

TSessionDone(c) ==
    /\ calive[c] /\ cpc[c] = "session"
    /\ chan[c] # "open"
    /\ cres' = [cres EXCEPT ![c] = chan[c]]   \* "ok" | "stopped" | "died"
    /\ cpc' = [cpc EXCEPT ![c] = "done"]
    /\ UNCHANGED <<conn, spawns, calive, opc, oexit, oalive, vmUp, spawnRes, lock,
                   phase, phaseRun, latest, work, stopping, termReq, chan, cmd,
                   cmdOwner, execCount, vmRan, dispatched, spc, rpc,
                   orphanAtSnapshot, execOnDraining, autoDiscardedAck, autoRecovered,
                   wedgedWithoutAck, crashes>>

\* kill -9 of a client: the kernel drops its flock (if it still holds it)
\* and the reservation pipe reaches EOF.
CrashClient(c) ==
    /\ CanCrash
    /\ calive[c] /\ cpc[c] \notin {"done", "crashed"}
    /\ calive' = [calive EXCEPT ![c] = FALSE]
    /\ cpc' = [cpc EXCEPT ![c] = "crashed"]
    /\ lock' = IF lock = ClientLock(c) THEN NoLock ELSE lock
    /\ spawnRes' = [spawnRes EXCEPT ![c] = FALSE]
    /\ crashes' = crashes + 1
    /\ UNCHANGED <<cres, conn, spawns, opc, oexit, oalive, vmUp, phase, phaseRun,
                   latest, work, stopping, termReq, chan, cmd, cmdOwner,
                   execCount, vmRan, dispatched, spc, rpc, orphanAtSnapshot,
                   execOnDraining, autoDiscardedAck, autoRecovered, wedgedWithoutAck>>

\* A client that finished exits; its reservation pipe reaches EOF (O6).
ClientExit(c) ==
    /\ calive[c] /\ cpc[c] = "done"
    /\ calive' = [calive EXCEPT ![c] = FALSE]
    /\ spawnRes' = [spawnRes EXCEPT ![c] = FALSE]
    /\ UNCHANGED <<cpc, cres, conn, spawns, opc, oexit, oalive, vmUp, lock, phase,
                   phaseRun, latest, work, stopping, termReq, chan, cmd, cmdOwner,
                   execCount, vmRan, dispatched, spc, rpc, orphanAtSnapshot,
                   execOnDraining, autoDiscardedAck, autoRecovered, wedgedWithoutAck, crashes>>

-----------------------------------------------------------------------------
(********************************* OWNER **********************************)

OUnchanged == UNCHANGED <<cpc, cres, spawns, calive, spc, rpc,
                          autoDiscardedAck, autoRecovered, wedgedWithoutAck, crashes>>

\* Create runs/<run_id>/ (clone of latest, or of the base image).
OBoot(i) ==
    /\ oalive[i] /\ opc[i] = "boot"
    /\ work' = [work EXCEPT ![i] = TRUE]
    /\ opc' = [opc EXCEPT ![i] = "wal"]
    /\ UNCHANGED <<oexit, oalive, vmUp, spawnRes, lock, phase, phaseRun, latest,
                   stopping, termReq, chan, cmd, cmdOwner, execCount, vmRan,
                   dispatched, orphanAtSnapshot, execOnDraining>>
    /\ UNCHANGED conn
    /\ OUnchanged

\* O2: phase := running BEFORE vm.start().
OWal(i) ==
    /\ oalive[i] /\ opc[i] = "wal"
    /\ phase' = "running"
    /\ phaseRun' = i
    /\ opc' = [opc EXCEPT ![i] = "vmStart"]
    /\ UNCHANGED <<oexit, oalive, vmUp, spawnRes, lock, latest, work, stopping,
                   termReq, chan, cmd, cmdOwner, execCount, vmRan, dispatched,
                   orphanAtSnapshot, execOnDraining>>
    /\ UNCHANGED conn
    /\ OUnchanged

OVmStart(i) ==
    /\ oalive[i] /\ opc[i] = "vmStart"
    /\ \E runs \in BOOLEAN :
          /\ vmUp' = [vmUp EXCEPT ![i] = runs]
          /\ vmRan' = (vmRan \/ runs)
          /\ opc' = [opc EXCEPT ![i] = IF runs THEN "agent" ELSE "bootFail"]
    /\ UNCHANGED <<oexit, oalive, spawnRes, lock, phase, phaseRun, latest, work,
                   stopping, termReq, chan, cmd, cmdOwner, execCount, dispatched,
                   orphanAtSnapshot, execOnDraining>>
    /\ UNCHANGED conn
    /\ OUnchanged

OAgent(i) ==
    /\ oalive[i] /\ opc[i] = "agent"
    /\ \E ok \in BOOLEAN : opc' = [opc EXCEPT ![i] = IF ok THEN "serving" ELSE "bootFail"]
    /\ UNCHANGED <<oexit, oalive, vmUp, spawnRes, lock, phase, phaseRun, latest,
                   work, stopping, termReq, chan, cmd, cmdOwner, execCount, vmRan,
                   dispatched, orphanAtSnapshot, execOnDraining>>
    /\ UNCHANGED conn
    /\ OUnchanged

\* O4: nothing was dispatched (not serving yet): drop the run, write stopped.
OBootFail(i) ==
    /\ oalive[i] /\ opc[i] = "bootFail"
    /\ work' = [work EXCEPT ![i] = FALSE]
    /\ phase' = "stopped"
    /\ phaseRun' = NoPid
    /\ vmRan' = FALSE
    /\ vmUp' = [vmUp EXCEPT ![i] = FALSE]
    /\ oalive' = [oalive EXCEPT ![i] = FALSE]
    /\ opc' = [opc EXCEPT ![i] = "exited"]
    /\ oexit' = [oexit EXCEPT ![i] = "bootFailed"]
    /\ lock' = NoLock
    /\ UNCHANGED <<spawnRes, latest, stopping, termReq, chan, cmd, cmdOwner,
                   execCount, dispatched, orphanAtSnapshot, execOnDraining>>
    /\ UNCHANGED conn
    /\ OUnchanged

\* O6 / O7: begin draining.
ODrain(i) ==
    /\ oalive[i] /\ opc[i] = "serving"
    /\ ((~Active(i) /\ ~spawnRes[i]) \/ termReq[i])
    /\ stopping' = [stopping EXCEPT ![i] = TRUE]
    /\ opc' = [opc EXCEPT ![i] = "draining"]
    /\ UNCHANGED <<oexit, oalive, vmUp, spawnRes, lock, phase, phaseRun, latest,
                   work, termReq, chan, cmd, cmdOwner, execCount, vmRan,
                   dispatched, orphanAtSnapshot, execOnDraining>>
    /\ UNCHANGED conn
    /\ OUnchanged

\* O7: snapshot only once nothing is in flight.
ODrained(i) ==
    /\ oalive[i] /\ opc[i] = "draining"
    /\ ~InFlight(i)
    /\ ~\E c \in Clients : chan[c] = "open" /\ conn[c] = i
    /\ opc' = [opc EXCEPT ![i] = "snap"]
    /\ UNCHANGED <<oexit, oalive, vmUp, spawnRes, lock, phase, phaseRun, latest,
                   work, stopping, termReq, chan, cmd, cmdOwner, execCount, vmRan,
                   dispatched, orphanAtSnapshot, execOnDraining>>
    /\ UNCHANGED conn
    /\ OUnchanged

\* O7: a stop terminates in-flight commands inside the guest.
OTerminate(c) ==
    /\ cmd[c] \in {"dispatched", "running"}
    /\ LET o == cmdOwner[c] IN
         /\ oalive[o] /\ vmUp[o] /\ opc[o] = "draining" /\ termReq[o]
    /\ cmd' = [cmd EXCEPT ![c] = "finished"]
    /\ chan' = [chan EXCEPT ![c] = IF @ = "open" THEN "stopped" ELSE @]
    /\ UNCHANGED <<cpc, cres, conn, spawns, calive, opc, oexit, oalive, vmUp,
                   spawnRes, lock, phase, phaseRun, latest, work, stopping,
                   termReq, cmdOwner, execCount, vmRan, dispatched, spc, rpc,
                   orphanAtSnapshot, execOnDraining, autoDiscardedAck, autoRecovered,
                   wedgedWithoutAck, crashes>>

GuestExec(c) ==
    /\ cmd[c] = "dispatched"
    /\ oalive[cmdOwner[c]] /\ vmUp[cmdOwner[c]]
    /\ cmd' = [cmd EXCEPT ![c] = "running"]
    /\ execCount' = [execCount EXCEPT ![c] = @ + 1]
    /\ UNCHANGED <<cpc, cres, conn, spawns, calive, opc, oexit, oalive, vmUp,
                   spawnRes, lock, phase, phaseRun, latest, work, stopping,
                   termReq, chan, cmdOwner, vmRan, dispatched, spc, rpc,
                   orphanAtSnapshot, execOnDraining, autoDiscardedAck, autoRecovered,
                   wedgedWithoutAck, crashes>>

GuestExit(c) ==
    /\ cmd[c] = "running"
    /\ oalive[cmdOwner[c]] /\ vmUp[cmdOwner[c]]
    /\ cmd' = [cmd EXCEPT ![c] = "finished"]
    /\ chan' = [chan EXCEPT ![c] = IF @ = "open" THEN "ok" ELSE @]
    /\ UNCHANGED <<cpc, cres, conn, spawns, calive, opc, oexit, oalive, vmUp,
                   spawnRes, lock, phase, phaseRun, latest, work, stopping,
                   termReq, cmdOwner, execCount, vmRan, dispatched, spc, rpc,
                   orphanAtSnapshot, execOnDraining, autoDiscardedAck, autoRecovered,
                   wedgedWithoutAck, crashes>>

\* Capture + commit: ONE replacement of the instance record sets the new
\* latest generation and phase = stopped (internals: SnapshotCommitTarget).
\* A failed capture leaves phase as it is (running/dirty) and the owner exits.
OSnap(i) ==
    /\ oalive[i] /\ opc[i] = "snap"
    /\ orphanAtSnapshot' = (orphanAtSnapshot \/ InFlight(i))
    /\ vmUp' = [vmUp EXCEPT ![i] = FALSE]
    /\ \E ok \in BOOLEAN :
          IF ok
            THEN /\ latest' = TRUE
                 /\ phase' = "stopped"
                 /\ phaseRun' = NoPid
                 /\ vmRan' = FALSE
                 /\ dispatched' = FALSE
                 /\ opc' = [opc EXCEPT ![i] = "cleanup"]
                 /\ UNCHANGED <<oexit, oalive, lock, conn>>
            ELSE /\ opc' = [opc EXCEPT ![i] = "exited"]
                 /\ oexit' = [oexit EXCEPT ![i] = "snapshotFailed"]
                 /\ oalive' = [oalive EXCEPT ![i] = FALSE]
                 /\ lock' = NoLock
                 /\ UNCHANGED <<latest, phase, phaseRun, vmRan, dispatched>>
                 /\ conn' = BreakConns(i)
    /\ UNCHANGED <<spawnRes, work, stopping, termReq, chan, cmd, cmdOwner,
                   execCount, execOnDraining>>
    /\ OUnchanged

\* Delete runs/<run_id>/ (now unreferenced) and exit; the kernel drops the lock.
OCleanup(i) ==
    /\ oalive[i] /\ opc[i] = "cleanup"
    /\ work' = [work EXCEPT ![i] = FALSE]
    /\ opc' = [opc EXCEPT ![i] = "exited"]
    /\ oexit' = [oexit EXCEPT ![i] = "ok"]
    /\ oalive' = [oalive EXCEPT ![i] = FALSE]
    /\ lock' = NoLock
    /\ UNCHANGED <<vmUp, spawnRes, phase, phaseRun, latest, stopping, termReq,
                   chan, cmd, cmdOwner, execCount, vmRan, dispatched,
                   orphanAtSnapshot, execOnDraining>>
    /\ conn' = BreakConns(i)
    /\ OUnchanged

\* kill -9 of an owner: VM dies, kernel releases the flock atomically.
CrashOwner(i) ==
    /\ CanCrash
    /\ oalive[i]
    /\ oalive' = [oalive EXCEPT ![i] = FALSE]
    /\ vmUp' = [vmUp EXCEPT ![i] = FALSE]
    /\ opc' = [opc EXCEPT ![i] = "exited"]
    /\ oexit' = [oexit EXCEPT ![i] = "crash"]
    /\ lock' = IF lock = OwnerLock(i) THEN NoLock ELSE lock
    /\ chan' = [c \in Clients |-> IF chan[c] = "open" /\ conn[c] = i THEN "died" ELSE chan[c]]
    /\ cmd' = [c \in Clients |-> IF cmdOwner[c] = i /\ cmd[c] \in {"dispatched", "running"}
                                   THEN "killed" ELSE cmd[c]]
    /\ conn' = BreakConns(i)
    /\ crashes' = crashes + 1
    /\ UNCHANGED <<cpc, cres, spawns, calive, spawnRes, phase, phaseRun,
                   latest, work, stopping, termReq, cmdOwner, execCount, vmRan,
                   dispatched, spc, rpc, orphanAtSnapshot, execOnDraining,
                   autoDiscardedAck, autoRecovered, wedgedWithoutAck>>

-----------------------------------------------------------------------------
(****************************** OTHER ACTORS ******************************)

\* O8: Stop RPC to the current lock holder.
Stop ==
    /\ spc = "idle"
    /\ lock.who = "owner"
    /\ oalive[lock.id]
    /\ termReq' = [termReq EXCEPT ![lock.id] = TRUE]
    /\ spc' = "done"
    /\ UNCHANGED <<cpc, cres, conn, spawns, calive, opc, oexit, oalive, vmUp,
                   spawnRes, lock, phase, phaseRun, latest, work, stopping, chan,
                   cmd, cmdOwner, execCount, vmRan, dispatched, rpc,
                   orphanAtSnapshot, execOnDraining, autoDiscardedAck, autoRecovered,
                   wedgedWithoutAck, crashes>>

\* O3: `lnx recover --discard-run` (user consent): only the crashed run is
\* dropped; latest stays.
Recover ==
    /\ rpc = "idle"
    /\ Wedged
    /\ work' = [work EXCEPT ![phaseRun] = FALSE]
    /\ phase' = "stopped"
    /\ phaseRun' = NoPid
    /\ vmRan' = FALSE
    /\ dispatched' = FALSE
    /\ rpc' = "done"
    /\ UNCHANGED <<cpc, cres, conn, spawns, calive, opc, oexit, oalive, vmUp,
                   spawnRes, lock, latest, stopping, termReq, chan, cmd, cmdOwner,
                   execCount, spc, orphanAtSnapshot, execOnDraining,
                   autoDiscardedAck, autoRecovered, wedgedWithoutAck, crashes>>

Next ==
    \/ \E c \in Clients :
          \/ TStart(c) \/ TLock(c) \/ TSpawn(c) \/ TWait(c) \/ TOpen(c)
          \/ TSessionDone(c) \/ CrashClient(c) \/ GuestExec(c) \/ GuestExit(c)
          \/ OTerminate(c) \/ ClientExit(c)
    \/ \E i \in Owners :
          \/ OBoot(i) \/ OWal(i) \/ OVmStart(i) \/ OAgent(i) \/ OBootFail(i)
          \/ ODrain(i) \/ ODrained(i) \/ OSnap(i) \/ OCleanup(i) \/ CrashOwner(i)
    \/ Stop
    \/ Recover

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------
(******************************** PROPERTIES *******************************)

TypeOK ==
    /\ cpc \in [Clients -> ClientPCs]
    /\ opc \in [Owners -> OwnerPCs]
    /\ phase \in {"stopped", "running", "dirty"}

AtMostOneOwner ==
    Cardinality({i \in Owners : oalive[i] /\ opc[i] \in HoldingPCs}) <= 1
AtMostOneVm == Cardinality({i \in Owners : vmUp[i]}) <= 1
\* Every running owner is the lock holder.
OwnerHoldsLock == \A i \in Owners : (oalive[i] /\ opc[i] \in HoldingPCs) => lock = OwnerLock(i)
NoExecOnDraining == ~execOnDraining
NoDoubleExec == \A c \in Clients : execCount[c] <= 1
\* The instance asks for `lnx recover` only when a command was dispatched
\* since the last commit (strictly stronger than OwnerLifecycle's
\* NoSpuriousWedge, which only requires that the guest ran).
NoSpuriousWedge == Wedged => dispatched
NoWedgeWithoutAck == ~wedgedWithoutAck
\* Recovery never silently drops a run that executed a client command.
NoAutoDiscardOfAckedState == ~autoDiscardedAck
\* A held lock always belongs to a live lnx process.
NoStuckInstance ==
    /\ lock.who = "owner" => oalive[lock.id]
    /\ lock.who = "client" => calive[lock.id]
NoOrphanAtSnapshot == ~orphanAtSnapshot
\* Clients only ever fail for a real reason.
NoSpuriousClientFailure ==
    \A c \in Clients : cres[c] \in {"none", "ok", "stopped", "died", "bootFailed",
                                    "crashedUnsaved", "bound"}
\* latest is never removed by any lifecycle or recovery action.
LatestMonotonic == [][latest => latest']_vars

\* Non-vacuity witnesses: each is EXPECTED to be violated (the state is
\* reachable), proving the invariants above are not true for trivial reasons.
WitnessBothClientsSucceed == ~(\A c \in Clients : cres[c] = "ok")
WitnessWedgeReachable == ~Wedged
\* A client took the lock after an owner crashed with phase = running and
\* removed that run automatically (no `lnx recover` involved).
WitnessAutoRecoveryAfterCrash == ~autoRecovered
WitnessStopTerminatesCommand == \A c \in Clients : cres[c] # "stopped" \/ chan[c] # "stopped"
\* After `lnx recover --discard-run` the instance can be locked again.
WitnessRecoverThenRun == ~(rpc = "done" /\ lock.who # "none")

=============================================================================
