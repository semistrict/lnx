# lnx UX redesign proposal

Tested on 0.3.0 (`LNX_BASE=/tmp/lnx-ux`). Cold create 10.4s, restore 0.3s, warm exec 16ms. Speed is great. Most friction comes from one design flaw: **boot-time machine shape is passed as per-run flags**.

## 1. Top 10 pain points

1. **Changing settings wedges the instance.**
   - `lnx set cpus=4; lnx nproc` → `snapshot VM config mismatch: snapshot_cpus=2 configured_cpus=4 … recovery: lnx --instance default snapshots clear`.
   - `--cpus 3` does the same.
   - On an instance created with `--no-host-shares`, a plain `lnx --instance sb pwd` → `share_mismatch: host-shares: snapshot=disabled current=enabled`.
2. **`--forward` doesn't attach to a running VM.** It waits 30s, then: `existing owner is still running … wait for it to checkpoint and exit`. When it does work, the port disappears once the VM sleeps (`curl` → `000`).
3. **Killing the client leaks the guest process and keeps the VM awake.**
   - After `pkill -TERM` on `lnx sleep 777`, the guest `sleep 777` keeps running and `default` stays `running` until killed manually.
   - `timeout(1)` and SDK `timeoutMs` (SIGKILL) hit the same path. Only SIGINT is handled.
4. **stdin isn't streamed.** `(echo a; sleep 2; echo b) | lnx sh -c 'while read l…'`: `a` and `b` arrive together at EOF (`…88.19`, `…88.20`). REPLs, LSP and MCP stdio servers don't work.
5. **No detach.** `lnx sh -c '(sleep 1000 &); echo started'` hangs after `started`. ComputeSDK works around it with a `nohup … &` string.
6. **Typos become guest commands or instances.**
   - `lnx instnaces list` → `command not found: instnaces`
   - `lnx --memroy-mib 8192 true` → `command not found: --memroy-mib`
   - `lnx run --instance t1 pwd` → `command not found: --instance`
   - `lnx --instance ghost fork g2` silently boots a new `ghost`, then forks it.
7. **Instance names aren't validated.** `--instance 'bad name'` creates it, but `instances delete 'bad name'` → `invalid instance name`. `--instance ../x paths` → `/tmp/lnx-ux/instances/../x`.
8. **A cwd outside `$HOME` only works for the instance's first boot directory.** `cd /private/tmp/…/proj; lnx pwd` → `working directory is not visible…; run: lnx fs unshare …` (exit 125). That advice is wrong.
9. **Sharing defaults are unsafe for sandboxes.**
   - The guest can read all of `$HOME` (`~/.ssh`, `~/.aws`). From `cd ~`, the write allowlist is `"."`: all of home is writable.
   - The ComputeSDK provider never sets `noHostShares`.
   - `fs unshare PATH` prints the same static `rule:` line for every path.
10. **Output and exit codes are inconsistent, and there's no rollback.**
    - Every listing has its own format (fixed-width `instances list`, headerless TSV `checkpoints`, `key: value` `paths`); `set` dumps JSON. The SDK scrapes all of them.
    - `inspect` on a missing instance → `"state": "partial"`, exit 0.
    - lnx's own errors exit 1, same as a guest `exit 1`.
    - Checkpoints can only be listed and deleted. Each `fork` leaves an unnamed internal checkpoint (3 listed after 2 forks).

Also:
- `init .` re-downloads 1.5 GB into `./.lnx/cache` and prints raw curl/zstd output.
- First run prints 11 `init:` lines.
- `logs` shows host trace lines, not guest output.
- The worktree auto-fork and upward `.lnx` search are silent.
- Idle timeout is only settable through `LNX_BROKER_IDLE_TTL_MS`.
- There is no `cp`, `-e`, `-w` or `-d`.

## 2. Mental model and command surface

**An instance is a persistent machine that sleeps.** Its configuration splits into two kinds, never mixed:

- **Shape**: cpus, memory, shares, ports, nested-kvm, idle-timeout. Saved, applied at cold boot, changed only via `config`.
- **Exec options**: per process.

Address instances as `@name`. No binary starts with `@`, so it can't collide with guest commands. `-n/--instance` and `LNX_INSTANCE` still work. Checkpoints are `name:ckpt` everywhere.

```
lnx [@NAME] [EXEC-OPTS] [--] CMD...      exec; no CMD = login shell
lnx run [@NAME] [EXEC-OPTS] [--] CMD...  explicit form; accepts flags after `run`
  -w, --workdir PATH   -e, --env K=V   --env-file F   -u USER | --root
  -d, --detach         -t/-T (PTY; default auto)      --timeout DUR

lnx create [NAME] [--from IMAGE|NAME[:CKPT]] [SHAPE]
lnx ls [--json]                      NAME STATE CPUS MEM DISK CKPTS BASE
lnx inspect [@NAME]                  JSON; absorbs `paths`
lnx config [@NAME] [KEY[=VAL]...]    no args = show (replaces `set`)
lnx stop | restart [@NAME]           restart = cold boot, drops memory
lnx rm [-f] NAME...
lnx ps [@NAME] / lnx kill [@NAME] PID|--all

lnx checkpoint [@NAME] [TAG] [-m MSG]
lnx checkpoints [@NAME] [--json] / checkpoints rm REF
lnx restore NAME:CKPT                roll back in place (new)
lnx fork SRC[:CKPT] NEW

lnx cp SRC DST                       @name:/path on either side
lnx port [@NAME] [add H:G | rm H]    persistent, wake-on-connect
lnx url [@NAME] PORT
lnx logs [@NAME] [-f] [--console|--trace]
lnx share [@NAME] [status [PATH] | reset PATH]   (was `fs unshare`)

lnx init [DIR] [--from REF]          shares the global image cache
lnx serve [--listen] / lnx push URL [--as NAME] [--start]
lnx ingress enable|disable|status|uninstall

SHAPE: --cpus N --memory 8G --share cwd|home|none --port H:G
       --nested-kvm --idle-timeout 5s
--help-all only: --kernel --rootfs --snapshot --deterministic
                 --trace-events --vhost-user-fs
Global: -C DIR  -q  -v  --json
Exit codes: 125 = lnx error, 126/127 = exec failure, 128+n = signal
```

Rules:

- **Resolution:**
  - Only `default` is created implicitly. Any other unknown name fails: `no instance 'ghost' (did you mean 'g2'?); lnx create ghost`.
  - Names must match `[a-z0-9][a-z0-9._-]*`.
  - An unknown pre-command `--flag` is an lnx error that suggests `--`.
  - A first token close to a verb that isn't on the guest PATH gets a suggestion.
- **Shape:**
  - `config @dev cpus=4` prints `applies at next cold boot; run lnx restart dev`.
  - The VM owner boots only from the saved shape, so a mismatch is impossible.
- **Sharing:**
  - Default `--share cwd`: project root (git root or cwd) read-write, gitignored paths copy-on-write, the rest of `$HOME` not mounted.
  - `--share home` brings back today's behavior, but `$HOME` itself is never the write root.
  - The cwd share is attached per exec, not frozen at boot.
  - SDK and ComputeSDK sandboxes default to `none`.
- **Process lifetime:** the client's death kills the guest process group (unless `-d`). SIGTERM and SIGHUP are forwarded. Detached processes don't keep the VM awake.
- **Output:** every listing supports `--json`, and the SDK uses only that. Progress goes to stderr as one line (`creating default from images-v0.6.0…`); `-v` shows detail. The active base is always visible.

Borrowed from other tools:

| Tool | What lnx takes |
|---|---|
| OrbStack | `orb CMD` shorthand |
| docker / Apple `container` | `ls`/`rm`/`ps`/`cp`/`-e`/`-w`/`-d`, exit 125 |
| Fly | clone → `fork` |
| E2B | `commands.run({background, envs, cwd, timeoutMs})` and `files.*` → `cp` |
| devcontainers | project-scoped `init` |

## 3. Migration

**Cheap and compatible** (old names become hidden aliases with a stderr deprecation note):
- `instances list` → `ls`
- `instances delete` → `rm`
- `set` → `config`
- `snapshots clear` → `restart`
- `paths` → `inspect`
- `fs unshare` → `share`
- `server [push]` → `serve`/`push`
- `--instance S fork N` still works

Also additive and non-breaking: `--json`, `@name`, `-e/-w/-d/--timeout`, `cp`, `port`, `restore`, `ps`/`kill`, name validation, exit 125, signal forwarding, stdin streaming.

**Breaking** (one release of warnings first):
- shape flags on exec become errors
- no implicit creation of non-default instances
- unknown flags are no longer passed to the guest
- default share becomes `cwd` (print a one-time notice)

## 4. Implementation list

**P0 (correctness and safety)**
1. Stream non-TTY stdin as it arrives. `src/runner.rs` client stdin pump, `guest-agent/src/main.rs`. Test: the timestamp pipe.
2. Kill the guest process group when the client dies; forward TERM and HUP. `src/runner.rs` `install_signal_handlers`, `src/runner/broker.rs` channel close. Test: `kill -TERM` the client → guest process gone and the instance goes idle.
3. Never wedge on a shape mismatch: exec uses the snapshot shape, and `set` warns and points to `restart`. `src/cli.rs` cpus/memory merge in `Cli::run` and `set_instance_settings`; launch-metadata check in `src/runner.rs`.
4. Validate names in `Layout::resolve` (`src/paths.rs`), reusing `validate_instance_name` from `src/server.rs`. Let `rm` delete legacy invalid names.
5. No implicit creation of non-default instances. Auto-init path in `src/cli.rs` `run_guest` and `fork_checkpoint`.
6. Exit 125 for lnx errors (`src/main.rs`).

**P1 (agents)**
7. `--json` for `ls` and `checkpoints`; switch `ts/index.ts` `parseInstances` and `parseCheckpoints` to it.
8. Exec options `-e/-w/-d/--timeout`: `RunArgs` in `src/cli.rs`, plus `lnx-protocol/`.
9. `lnx cp` over the agent channel, replacing the base64 shell calls in `ts/computesdk/src/index.ts`.
10. ComputeSDK: `noHostShares` by default; `background` maps to `-d`.
11. Clap: allow flags after `run`, reject unknown flags, add typo suggestions (`Cli` in `src/cli.rs`).
12. `restore`, positional `fork SRC NEW`, hide internal fork checkpoints (`src/checkpoints.rs`).

**P2 (model and polish)**
13. `--share` setting, cwd attached per exec, `$HOME` never the write root: `home_write_allowlist` in `src/runner.rs`, plus `src/host_share.rs`.
14. Persistent `port` with wake-on-connect, reusing ingress `start_instance` (`src/ingress.rs:1764`).
15. Quiet init and a shared image cache for `init DIR` (`src/init.rs`).
16. `logs` defaults to the guest journal (`print_instance_logs`).
17. Verb renames with hidden aliases, plus `--help-all` (`Command` in `src/cli.rs`).
18. `idle-timeout` config key, replacing `LNX_BROKER_IDLE_TTL_MS`.
