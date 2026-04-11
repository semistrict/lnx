# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is lnx

lnx is a lightweight Linux VM runner for macOS using Apple's Virtualization.framework. It boots a Linux kernel directly (no UEFI/bootloader), runs a custom init binary as PID 1, and communicates between host and guest over vsock.

## Build & Test Commands

```bash
make                  # Build host binary (cross-compiles guest init, embeds it, codesigns)
make install          # Install to $GOPATH/bin
make test             # Unit tests (any platform)
make test-integration # Integration tests (macOS only, needs ~/.lnx/vmlinuz + rootfs.ext4)
make test-integration RUN=TestName  # Run a single integration test
make kernel           # Build Linux kernel in Docker
make rootfs           # Build rootfs ext4 image in Docker
```

The guest init is cross-compiled (`CGO_ENABLED=0 GOOS=linux GOARCH=arm64`) and embedded into the host binary via `//go:embed`. The host binary must be codesigned with virtualization entitlements. Integration tests use a codesign wrapper (`cmd/codesign/`) that signs the test binary before execution.

## Architecture

### Two binaries, one repo

- **Host binary** (`cmd/lnx/`): macOS CLI that creates and manages VMs via `github.com/Code-Hex/vz/v3` (Go bindings for Virtualization.framework).
- **Guest init** (`cmd/init/`): Linux binary that runs as PID 1 inside the VM. All files have `//go:build linux`.

`go.mod` replaces `Code-Hex/vz/v3` with the `semistrict/vz` fork. A gitignored `go.work` file overrides this with the local `third_party/vz/` submodule for development. If you need to modify the vz bindings, work in `third_party/vz/`.

### Host ↔ Guest communication over vsock

All communication uses virtio-vsock (no serial console for I/O). Ports are defined in `internal/protocol/protocol.go`:

| Port | Purpose | Encoding |
|------|---------|----------|
| 1024 | Control (setup; VM-level lifecycle only) | gob |
| 1025 | Guest → host logging | JSON lines |
| 1026 | Status queries | gob |
| 1027 | Exec commands + per-session signals/resize (host connects per session) | gob |
| 1028 | Guest → host requests (checkpoint, open URL) | gob |
| 1030 | Port forward notifications | gob |
| 1031 | Port forward data (host connects via `VirtioSocketDevice.Connect`) | raw bytes with 2-byte port header |
| 1032 | Interactive exec PTY I/O (host connects via `VirtioSocketDevice.Connect`) | raw bytes |
| 1033 | 9P file server (host home dir, read-only) | 9P2000.L |
| 1034 | SSH agent forwarding (host listens, guest dials) | SSH agent protocol |
| 1035 | Guest HTTP debug endpoints (host → guest) | HTTP |

The `Msg` envelope in protocol.go has exactly one non-nil field per message.

### VM lifecycle — daemon model (vm.go)

The VM runs as a background daemon process. All `lnx <command>` invocations are exec clients.

1. Client checks `~/.lnx/instances/<name>/status.sock` for a running daemon
2. If no daemon: client spawns `lnx _daemon --instance <name>` in the background, waits for `status.sock`
3. Daemon boots: lock rootfs (flock), write initramfs, build VM config, start VM
4. Guest init boots, dials host on port 1024, receives `Setup` message
5. Guest starts services (exec server, status server, port forwarder)
6. Daemon listens on `status.sock`
7. Client connects via WebSocket (`GET /exec/ws`) or HTTP (`POST /exec`)
8. Multiple clients can exec concurrently — each gets its own vsock connection on port 1027
9. Signals/resize are per-session via `ExecSignal`/`ExecResize` messages on the gob connection
10. Interactive I/O uses WebSocket: binary frames = PTY data, text frames = signals/resize/exit_code
11. When all exec sessions finish (active count → 0), daemon shuts down automatically
12. `lnx stop` can also shut down the daemon via `POST /stop`

### Filesystem mounts

- **CWD**: virtiofs share, mounted read-write in the guest at the same path as the host.
- **Home directory**: 9P over vsock (port 1033), mounted read-only. Host serves via `hugelgupf/p9` localfs. Mount failure is non-fatal.

### 9P security filtering (p9filter.go)

The home directory 9P share blocks sensitive paths via `blockedDirs` in p9filter.go (`.ssh`, `.gnupg`, `.aws`, `.docker`, `.kube`, browser profiles, keychains, etc.). Walk and Readdir calls return `EACCES` for blocked paths. CWD and extra virtiofs shares have no filtering.

### Guest networking (internal/lnxnet/)

Pure Go network stack used by the guest init: ARP, DHCP, ethernet frame handling, IPv4, TCP, UDP, and bridge. No CGO, runs inside the VM.

### Port forwarding (portfwd.go, cmd/init/portfwd.go)

Guest scans `/proc/net/tcp` every 2s for listening ports, sends updates to host on port 1030. Host binds `127.0.0.1:<port>` and forwards TCP connections to the guest using `VirtioSocketDevice.Connect` on port 1031.

### Host API server (status.go)

HTTP server on `~/.lnx/instances/<name>/status.sock` (unix socket) exposes:
- `GET /status` — VM status (uptime, memory, disk, load)
- `GET /ports` — forwarded ports
- `POST /exec` — non-interactive exec (NDJSON streaming response)
- `GET /exec/ws` — interactive exec over WebSocket (binary frames = PTY, text frames = control)
- `POST /stop` — shut down the daemon

CLI commands (`lnx status`, `lnx ports list`, `lnx stop`) are thin HTTP clients. `lnx <command>` uses `/exec/ws` for interactive or `/exec` for non-interactive.

## Test structure

- **Unit tests** (`*_test.go`): No build tags, run anywhere, test protocol logic and utilities.
- **Integration tests** (`*_intg_test.go`): Tagged `//go:build darwin && integration`, each test boots a real VM. Use `setupTestDir(t)` which clones rootfs via APFS clonefile. Most use `t.Parallel()`.
- **ForceQuit test** is NOT parallel because it sends `SIGINT` to the process.

## Testing requirements

A feature is NOT complete until it has tests. Do not declare a feature done without them.

- **Unit tests**: For any new protocol messages, config parsing, or pure logic.
- **Integration tests (midterm)**: For any user-facing behavior that involves the VM. PTY tests use `creack/pty` + `vito/midterm` to simulate a real terminal. Use `--ephemeral` or `LNX_INSTANCE=test-xxx` with a cloned rootfs to avoid rootfs lock contention with other parallel tests.
- **Run the test before declaring it works**. Use `make install` first for PTY/midterm tests (they exec the `lnx` binary from PATH). Run the specific test in isolation first (`make test-integration RUN=TestName`), then the full suite.
- If a feature touches host↔guest communication (new vsock port, new Setup field, new guest service), the integration test must verify end-to-end behavior through the actual VM, not just the host side.

## When adding fields to Config or protocol.Setup

- `Config` fields are manually copied in the ephemeral path (`vm.go`). If you add a field, you MUST update that copy. `TestConfig_AllFieldsCopied` will fail if you forget — run `make test` first.
- `protocol.Setup` fields are gob-encoded. New fields work automatically, but test the round-trip with `TestControlProtocol_SetupDelivered`.

## Debugging failures

When a test or feature doesn't work, check the **data flow** first, not the build system:
- Trace the value through each layer: CLI flag → Config → Setup message → guest init
- Check if there's a manual struct copy that drops fields (ephemeral path, gob re-encoding)
- Do NOT spend time on build cache issues (`go clean -cache`, MD5 comparisons, binary extraction) until you've ruled out logic bugs

## Bash command timeouts — CRITICAL

ALWAYS set the `timeout` parameter on EVERY Bash tool call. Never rely on defaults.
- Quick commands (ls, git status, go build): 30000 (30s)
- Medium commands (make, unit tests): 60000 (60s)
- Integration tests: 120000 (120s) MAX
- NEVER use 600000 (10 min) — if a command needs that long, use `run_in_background: true`
- If a command might hang (VM operations, network), use a SHORT timeout

## Key conventions

- All interactive I/O goes through WebSocket (`handleExecWS` in status.go). Binary frames carry PTY data, text frames carry JSON control messages (signals, resize, exit_code).
- Each exec gets its own vsock connection (host connects to guest per session on port 1027), so multiple execs can run concurrently.
- Signals and resize are per-session via `ExecSignal`/`ExecResize` gob messages on port 1027, not the control connection.
- Double Ctrl-C force-quits the current session (exit 130). If it was the last session, the daemon shuts down.
- The CLI bypasses cobra for guest commands: if the first arg isn't a known subcommand or flag, it goes directly to `runVM()` so flags like `-g` pass through to the guest.
- Guest commands that aren't found print `name: command not found` (exit 127).
- `LNX_LOG=debug` enables host-side debug logging to `~/.lnx/lnx.log`.
- The `_daemon` subcommand is hidden/internal — never invoke it directly. It's spawned by `runVM()` when no VM is running.
