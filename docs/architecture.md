# Architecture

`lnx` boots a Linux kernel directly with [libkrun](https://github.com/containers/libkrun),
uses a normal systemd rootfs, and preserves VM memory plus disk state with
libkrun snapshots between commands.

## Real systemd root

The important architectural difference from libkrun's simple `krun_set_root`
examples is that `lnx` keeps using the existing `rootfs.ext4` as the real
systemd root:

1. `krun_add_disk(ctx, "rootfs", rootfs.ext4, false)` attaches the rootfs image.
2. The host generates an initramfs containing `lnx-agent` as both `/init` and
   `/lnx-agent`.
3. libkrun's bootstrap init execs `/init --init`.
4. `/init --init` mounts `/dev/vda` at `/newroot`, copies `/lnx-agent` into
   `/newroot/usr/local/lib/lnx/lnx-agent`, writes a systemd unit in
   `/newroot/etc/systemd/system`, then `chroot`s and execs `/sbin/init`.
5. Linux userspace therefore comes from the existing ext4 image, not from a
   host-directory virtiofs root.

## Exec flow

1. The Rust build script compiles `guest-agent/src/main.rs` into a static Linux
   binary named `lnx-agent`.
2. Before boot, the host writes an initramfs containing that binary.
3. The binary's `--init` mode stages itself into `/usr/local/lib/lnx` in the
   real root.
4. systemd starts `lnx-agent --agent 10240`.
5. The host connects to `lnx-agent` over libkrun's vsock-to-Unix-socket port
   mapping, sends one argv vector, streams stdout/stderr frames, and exits with
   the guest command status.

## Instance state

Each instance keeps its state under `~/.lnx/instances/<instance>/` as
immutable *generations* plus one record naming the latest one:

```text
state.json                 {latest generation, phase: stopped | running | dirty}
generations/<gen>/         rootfs.ext4, [vmstate.bin pages.img stamps], host-share-state/, manifest.json
runs/<run>/                private clones the running VM works on
checkpoints/<id>.ref       a named reference to a generation
instance.lock              flock held by the VM owner or a maintenance command
```

A run starts by cloning the latest generation into `runs/<run>/` (APFS
clones, so this is instant) and recording `phase: running`. Before the first
command reaches the guest the record becomes `dirty`. The VM runs in a
detached `_vm-owner` process, so `lnx` exits as soon as the guest command's
status arrives; the owner keeps the VM alive for an idle grace period (5s by
default) so rapid-fire commands reuse it. Once idle, it asks the guest to
quiesce, captures memory, disk and host-share state at one paused instant
into a new generation, syncs it, and commits `{latest: new, phase: stopped}`
as the single commit point. The next command resumes from there. Restored
runs use libkrun dirty tracking to write only changed RAM.

If an owner dies, the next command recovers before starting a VM: a run that
had published its final snapshot is rolled forward, and a run that never
served a command is dropped. A run that crashed after serving commands may
hold writes a client was told about, so lnx stops and asks:
`lnx recover --keep` keeps its disk (dropping only its memory), and
`lnx recover --discard` returns to the last saved state. Nothing deletes the
latest generation.

`lnx snapshots clear` drops the saved memory, never the disk: the next run
boots from the saved disk. Checkpoints of a stopped instance are references
to its latest generation; checkpoints of a running one are live captures.
Forks copy a generation into a new instance. Instances created by older lnx
versions are migrated into this layout on first use.

The protocol is modeled in TLA+ under `specs/tla/` (`bun run tla:check`).

Per-run timings are appended to `~/.lnx/instances/<instance>/timings.log`.

Host shares always mount with virtio-fs DAX. A memory snapshot records the
share layout, guest agent and VM shape it needs; a run that cannot resume it
fails with an explanation and the `snapshots clear` remedy instead of silently
booting.

## Filesystem

The rootfs is not a virtual disk in the usual sense. The ext4 image (a sparse
file on APFS) is mapped into the guest as a virtio-pmem device and mounted
with `rootflags=dax`, so guest file access faults directly onto the
host-mapped pages of the image — there is no block-I/O path and no guest page
cache duplicating host memory. That is what keeps memory snapshots small
(cache pages never exist in guest RAM) and makes disk state instant to clone:
checkpoints and forks copy the image with APFS `clonefile`, sharing all
blocks copy-on-write.

Host directories (the home directory and the working directory the command
started in) are exported over virtio-fs with DAX and mounted at their host
paths inside the guest, so absolute paths work unchanged on both sides. Guest
writes to shared trees pass through only for allowlisted paths; everything
else is diverted into per-instance copy-on-write state (upper files and
whiteouts, saved with each generation), so
a guest can never mutate the host tree outside the allowlist. `lnx fs
unshare` lists and clears that state. Additional read-only virtio-fs mounts
from external vhost-user backends attach with `--vhost-user-fs`.

## Networking

Networking uses [gvisor-tap-vsock](https://github.com/containers/gvisor-tap-vsock)
via libkrun's `krun_add_net_unixgram` backend. The Go network stack is
statically linked into the `lnx` binary (`third_party/gvproxy-bridge`); no
external gvproxy is needed.

## Ingress

`lnx ingress enable` installs a `.lnx` resolver, starts local HTTP and HTTPS
listeners, and trusts a local, name-constrained `lnx` CA in the macOS System
keychain. HTTPS certificates are generated per `.lnx` host on first use and
terminate at the host ingress before proxying plain HTTP/WebSocket traffic to
the guest port. See [security.md](security.md) for exactly what ingress
installs and how to remove it.

## Guest images

The managed rootfs image ships with a development toolchain baked in: the
latest Node.js (node/npm/npx, from the official nodejs.org tarball) plus pnpm
in `/usr/local`, alongside the Ubuntu userland. Install anything else with
`apt-get` inside the guest; each instance's rootfs is persistent.

## Nested KVM

```sh
bun run test:nested-kvm
```

The nested test compiles `lnx` for `aarch64-unknown-linux-musl`, boots an outer
`lnx --nested-kvm` guest, verifies that an inner `lnx` VM can boot after the
outer VM has gone through `lnxctl snapshot-exit`, then runs the Linux-host
compatible part of the integration suite inside the nested-capable guest.

### Current caveats

- Inner nested `lnx` runs use `LNX_ROOTFS_BACKEND=block`; pmem/DAX rootfs inside
  the nested Linux host still hits KVM mapping limitations.
- Linux libkrun snapshot APIs are wired for a full-RAM KVM/aarch64 capture and
  restore path. Incremental dirty-log snapshots are not implemented yet, so the
  Linux path is expected to be correct but heavier than the macOS/HVF path until
  it grows KVM dirty-log support.
- Linux virtiofs write allowlist enforcement is not active today, so the
  policy-specific virtiofs restore/fork checks do not run inside the nested
  Linux host.

## Vendored libkrun

`lnx` builds against the copy of libkrun vendored in-tree at
`third_party/libkrun`. It carries patches adding memory snapshot
capture/restore with dirty tracking on macOS/HVF, which
[upstream libkrun](https://github.com/containers/libkrun) does not have.
`CC_LINUX` is needed at build time because libkrun compiles its own embedded
Linux init helper.
