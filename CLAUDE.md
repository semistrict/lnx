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

- **Host binary** (`cmd/lnx/`): macOS CLI that creates and manages VMs. Uses `github.com/Code-Hex/vz/v3` (fork at `github.com/semistrict/vz`, local checkout in `third_party/vz/`). `go.mod` points to the remote fork; a gitignored `go.work` overrides with the local submodule for development.
- **Guest init** (`cmd/init/`): Linux binary that runs as PID 1 inside the VM. All files have `//go:build linux`.

### Host ↔ Guest communication over vsock

All communication uses virtio-vsock (no serial console for I/O). Ports are defined in `internal/protocol/protocol.go`:

| Port | Purpose | Encoding |
|------|---------|----------|
| 1024 | Control (setup, signals, resize) | gob |
| 1025 | Guest → host logging | JSON lines |
| 1026 | Status queries | gob |
| 1027 | Exec commands (host connects per session via `VirtioSocketDevice.Connect`) | gob |
| 1028 | Guest → host requests (checkpoint, open URL) | gob |
| 1030 | Port forward notifications | gob |
| 1031 | Port forward data (host connects via `VirtioSocketDevice.Connect`) | raw bytes with 2-byte port header |
| 1032 | Interactive exec PTY I/O (host connects via `VirtioSocketDevice.Connect`) | raw bytes |
| 1033 | 9P file server (host home dir, read-only) | 9P2000.L |

The `Msg` envelope in protocol.go has exactly one non-nil field per message.

### VM lifecycle (vm.go)

1. Lock rootfs (flock), optionally checkpoint (APFS clonefile)
2. Write initramfs from embedded init binary
3. Build VM config: kernel, initrd, serial→/dev/null, disks, virtiofs shares, vsock, NAT network
4. Set terminal raw mode (interactive), query initial window size
5. Start VM, set up vsock listeners for all ports
6. Guest init boots, dials host on port 1024, receives `Setup` message (user, env, cwd)
7. Guest mounts rootfs, sets up user, starts services (exec server, status server, port forwarder)
8. Host sends `ExecReq` on port 1027; interactive I/O flows over port 1032; signals/resize over port 1024
9. Host closes control connection → guest powers off

### Filesystem mounts

- **CWD**: virtiofs share, mounted read-write in the guest at the same path as the host.
- **Home directory**: 9P over vsock (port 1033), mounted read-only. Host serves via `hugelgupf/p9` localfs. Mount failure is non-fatal.

### Port forwarding (portfwd.go, cmd/init/portfwd.go)

Guest scans `/proc/net/tcp` every 2s for listening ports, sends updates to host on port 1030. Host binds `127.0.0.1:<port>` and forwards TCP connections to the guest using `VirtioSocketDevice.Connect` on port 1031.

### Host API server (status.go)

HTTP server on `~/.lnx/status.sock` (unix socket) exposes `GET /status`, `GET /ports`, `POST /exec`. CLI commands (`lnx status`, `lnx exec`, `lnx ports list`) are thin HTTP clients.

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

## Key conventions

- `spliceInteractive` in status.go is the single unified path for interactive I/O — used by both the main command and `lnx exec -i` (via HTTP hijack).
- Each exec gets its own vsock connection (host connects to guest per session on port 1027), so multiple execs can run concurrently.
- Double Ctrl-C force-stops the VM (exit 130). Single Ctrl-C forwards SIGINT to guest.
- The CLI bypasses cobra for guest commands: if the first arg isn't a known subcommand or flag, it goes directly to `runVM()` so flags like `-g` pass through to the guest.
- Guest commands that aren't found print `name: command not found` (exit 127).
- `LNX_LOG=debug` enables host-side debug logging to `~/.lnx/lnx.log`.
