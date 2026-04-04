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

- **Host binary** (`cmd/lnx/`): macOS CLI that creates and manages VMs. Uses `github.com/Code-Hex/vz/v3` (local fork in `third_party/vz/`).
- **Guest init** (`cmd/init/`): Linux binary that runs as PID 1 inside the VM. All files have `//go:build linux`.

### Host ↔ Guest communication over vsock

All communication uses virtio-vsock (no serial console for I/O). Ports are defined in `internal/protocol/protocol.go`:

| Port | Purpose | Encoding |
|------|---------|----------|
| 1024 | Control (exec command, signals, resize, exit handshake) | gob |
| 1025 | Guest → host logging | JSON lines |
| 1026 | Status queries | gob |
| 1027 | `lnx exec` commands | gob |
| 1028 | Guest → host requests (checkpoint, open URL) | gob |
| 1029 | Terminal I/O (stdin/stdout for the running command) | raw bytes |
| 1030 | Port forward notifications | gob |
| 1031 | Port forward data (host connects to guest via `VirtioSocketDevice.Connect`) | raw bytes with 2-byte port header |

The `Msg` envelope in protocol.go has exactly one non-nil field per message.

### VM lifecycle (vm.go)

1. Lock rootfs (flock), optionally checkpoint (APFS clonefile)
2. Write initramfs from embedded init binary
3. Build VM config: kernel, initrd, serial→/dev/null, disks, virtiofs shares, vsock, NAT network
4. Set terminal raw mode (interactive), query initial window size
5. Start VM, set up vsock listeners for all ports
6. Guest init boots, dials host on port 1024, receives `Exec` message
7. Guest mounts rootfs, sets up user, starts services, runs command
8. Terminal I/O flows over port 1029; signals/resize over port 1024
9. Guest sends `Exit` with code, host sends `Ack`, VM powers off

### Port forwarding (portfwd.go, cmd/init/portfwd.go)

Guest scans `/proc/net/tcp` every 2s for listening ports, sends updates to host on port 1030. Host binds `127.0.0.1:<port>` and forwards TCP connections to the guest using `VirtioSocketDevice.Connect` on port 1031.

### Host API server (status.go)

HTTP server on `~/.lnx/status.sock` (unix socket) exposes `GET /status`, `GET /ports`, `POST /exec`. CLI commands (`lnx status`, `lnx exec`, `lnx ports list`) are thin HTTP clients.

## Test structure

- **Unit tests** (`*_test.go`): No build tags, run anywhere, test protocol logic and utilities.
- **Integration tests** (`*_intg_test.go`): Tagged `//go:build darwin && integration`, each test boots a real VM. Use `setupTestDir(t)` which clones rootfs via APFS clonefile. Most use `t.Parallel()`.
- **ForceQuit test** is NOT parallel because it sends `SIGINT` to the process.

## Key conventions

- `runDirect` uses `StdinPipe` (not `cmd.Stdin = vsockConn`) to avoid `cmd.Wait()` hanging on the never-closing terminal vsock.
- Double Ctrl-C force-stops the VM (exit 130). Single Ctrl-C forwards SIGINT to guest.
- The CLI bypasses cobra for guest commands: if the first arg isn't a known subcommand or flag, it goes directly to `runVM()` so flags like `-g` pass through to the guest.
- Guest commands that aren't found print `name: command not found` (exit 127).
- `LNX_LOG=debug` enables host-side debug logging to stderr.
