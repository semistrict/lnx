# Release Notes

## Mach-O section injection for `lnx pack`

`lnx pack` now embeds the kernel and rootfs in a proper `__LNX,__lnxpack` Mach-O section instead of appending bytes after the code signature. The packed binary is a valid Mach-O that can be re-signed with `codesign`. A CGo placeholder section is created at build time; the pack command replaces it with the compressed payloads.

## Docker containers and `lnx clone --image`

`lnx docker run` now creates ephemeral containers with APFS-cloned rootfs images, cleaned up on exit. Docker-style `-p`/`-P` port mapping and `lnx docker ps` are supported.

`lnx clone --image <ref> <name>` creates a persistent instance from an OCI image.

OCI pull/build logic has been extracted to `internal/lnxoci`.

## Sync Shares

Host directories can now be shared with near-native ext4 speed using lazy-cache FUSE overlays. Files are copied into the guest's rootfs on first access and served from ext4 on subsequent reads. A background goroutine keeps the cache fresh within ~5 seconds of host-side changes.

```bash
lnx sync add ~/src/myrepo
```

The home directory (`$HOME`) also uses this mechanism automatically, replacing the previous 9P mount. Sensitive paths (`.ssh`, `.gnupg`, `.aws`, etc.) remain blocked.

**Performance** (`git status` on a ~3,600-file repo):

| | virtiofs (before) | Sync share |
|-|-------------------|------------|
| Cold cache | 4.7s | 1.4s |
| Warm cache | 4.7s | 0.05s |

See [docs/sync-shares.md](docs/sync-shares.md) for the full design.

## SSH access to lnx VMs

`ssh <instance>.lnx` now works out of the box. An embedded SSH server runs inside the guest init (PID 1) over vsock — no sshd installation required.

### How it works

On `lnx init`, a `Host *.lnx` block is added to `~/.ssh/config` with a `ProxyCommand` that routes through `lnx _ssh-proxy`. When you run `ssh foo.lnx`:

1. The proxy command auto-starts the VM if it isn't running
2. The host daemon opens a vsock connection to the guest's embedded SSH server (port 1040)
3. SSH protocol flows end-to-end over vsock — key exchange, auth, channels, PTY all work normally

Commands run as the same user, in the same CWD, with the same environment as `lnx exec`. PTY, window resize, scp, and sftp all work.

### SSH config (installed automatically by `lnx init`)

```
Host *.lnx
  ProxyCommand lnx _ssh-proxy %h %p
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
```

## CRIU Checkpoints and VM Fork

This release adds process-level checkpoint/restore using CRIU (Checkpoint/Restore In Userspace), enabling fast snapshotting and forking of running VMs.

### New features

**CRIU checkpoints** (`lnx checkpoints create --criu <name>`)

Captures the full state of all running processes — memory, file descriptors, TCP connections, pipes, Unix domain sockets — alongside the disk. Restore with `lnx checkpoints restore <name>` to roll back a VM to the exact moment of the checkpoint, including in-flight network connections and in-memory state.

**VM fork** (`lnx fork`)

Clones a running VM into an independent copy. The child VM boots with all processes restored to the same state as the parent at the moment of the fork. The parent continues uninterrupted.

**Guest-initiated fork** (pipe-based, like `fork()`)

Processes inside the VM can trigger a fork by writing to fd 3 and reading the result from fd 4. In the parent, the read returns the child instance name. In the CRIU-restored child, the read returns EOF. See `examples/fork.py` for the pattern.

### Changes

- CRIU images are stored on a dedicated block device (`criu.ext4`), separate from the rootfs. This keeps checkpoint data out of the root filesystem and enables independent cloning.
- CRIU is now built from a local fork (`third_party/criu`) that patches `SO_PASSSEC` handling for kernels without LSM support.
- The rootfs Dockerfile (`Dockerfile.rootfs`) builds CRIU from the local fork instead of cloning upstream.
- Process-level checkpoint/restore is listed alongside disk-only checkpoints in `lnx checkpoints list` (shown as type `criu` vs `disk`).
- The `lnx fork` command can optionally exec into the child VM with `lnx fork -- <command>`.
- Leaked file descriptors (vsock sockets from the VZ framework) are no longer inherited by guest processes, fixing CRIU dump failures on processes that don't explicitly close inherited fds.
- CRIU auto-restore on boot now runs before any process-forking commands (network setup, resize2fs), preventing PID conflicts that would cause restore failures.
