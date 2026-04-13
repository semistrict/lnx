# Sync Shares

Sync shares give the guest near-native filesystem speed for host directories.
They replace the default virtiofs/9P mounts with a FUSE lazy-cache overlay:
every file is copied into the guest's ext4 rootfs on first access and served
from there on subsequent reads. A background goroutine keeps the cache fresh.

## Quick start

```bash
lnx sync add ~/src/myrepo    # persisted, takes effect on next boot
lnx sync list
lnx sync remove ~/src/myrepo
```

The shared directory appears at the same absolute path inside the VM
(e.g. `~/src/myrepo` on the host is `/Users/you/src/myrepo` in the guest).

## How it works

```
guest reads /repo/src/foo.go
  -> FUSE (lazyCacheFS) at /repo
  -> check /var/lnx/cache/sync0/src/foo.go  (ext4, fast)
  -> cache miss: read /var/lnx/lower/sync0/src/foo.go  (virtiofs, one-time cost)
  -> copy to ext4 cache, serve from cache
  -> next read: pure ext4, zero virtiofs overhead
```

### Layers

| Layer | Path in guest | Filesystem | Access |
|-------|--------------|------------|--------|
| Lower | `/var/lnx/lower/sync<i>` | virtiofs (read-only) | Host directory, unchanged |
| Cache | `/var/lnx/cache/sync<i>` | ext4 (rootfs) | Copy-on-first-read, writable |
| FUSE  | Original host path | FUSE overlay | What the guest process sees |

### Cache-first lookups

Every `Getattr` and `Lookup` call checks the ext4 cache first. If the file
exists in cache, the result is returned without touching virtiofs. This
eliminates the host round-trip that makes virtiofs slow for metadata-heavy
workloads like `git status`.

### Kernel-level caching

FUSE entry and attribute results are cached by the Linux kernel for 5 seconds
(`EntryTimeout` / `AttrTimeout`). Within that window, repeated access to the
same file doesn't even enter the FUSE server — the kernel serves it directly.

### Background refresh

A goroutine walks the cache every 5 seconds. For each cached file, it
compares the lower (virtiofs) mtime with the cache mtime. If the host copy
is newer, the cache is updated. This means host-side edits appear in the
guest within ~5 seconds without any guest-side action.

### Write semantics

Writes from inside the guest go to the ext4 cache only. The host directory
is mounted read-only via virtiofs and is never modified by the guest. This
means:

- Guest writes are fast (native ext4).
- Guest writes do **not** appear on the host.
- Guest writes persist across reboots (they live in the rootfs).

## Home directory

The home directory (`$HOME`) uses the same lazy-cache mechanism automatically.
No `lnx sync add` is needed — it is always mounted as a FUSE overlay with
the same cache-first behavior.

The home mount includes a blocked-path filter that hides sensitive directories
from the guest (`.ssh`, `.gnupg`, `.aws`, `.docker`, `.kube`, browser profiles,
keychains, etc.). Attempts to access blocked paths return `EACCES`.

## Performance

Measured on a ~3,600-file repository (`git status`):

| Scenario | virtiofs (before) | Sync share |
|----------|-------------------|------------|
| Cold cache (first run) | 4.7s | 1.4s |
| Warm cache (second run) | 4.7s | 0.05s |

The warm-cache improvement comes from two things:
1. **Cache-first lookups** skip virtiofs entirely for cached files.
2. **Kernel cache** (5s TTL) skips the FUSE server entirely for repeated access.

## Implementation details

### FUSE inode stability

FUSE inodes use deterministic IDs derived from the file path (FNV-64a hash).
This ensures that when the kernel's entry cache expires and it re-lookups a
path, the FUSE server returns the same node ID. Without this, `getcwd()`
fails after cache expiry because the kernel's dentry tree points to stale
node IDs.

### Protocol

The `Setup` message includes a `SyncShares []string` field listing host paths.
The host attaches each sync share as a read-only virtiofs device tagged
`sync0`, `sync1`, etc. The guest init mounts these to lower directories
pre-pivotRoot, then starts FUSE servers post-pivotRoot.

### Files

| File | Role |
|------|------|
| `cmd/init/lazyfuse.go` | FUSE filesystem: Lookup, Getattr, Open, Create, Readdir, etc. |
| `cmd/init/mount.go` | Pre-pivotRoot: mount virtiofs lower + create cache dirs |
| `cmd/init/main.go` | Post-pivotRoot: start FUSE servers and refresh goroutines |
| `cmd/lnx/sync_cmd.go` | CLI: `lnx sync add/remove/list` |
| `cmd/lnx/daemon_cmd.go` | Load sync-shares.json into Config |
| `devices_darwin.go` | Attach virtiofs devices (home, cwd, shares, sync shares) |
| `vm.go` | Thread SyncShares through Setup message |
