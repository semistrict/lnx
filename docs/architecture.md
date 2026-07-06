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

## Snapshot lifecycle

Memory snapshot restore defaults to
`~/.lnx/instances/<instance>/memory-snapshots/latest` and can be overridden
with `--snapshot <dir>`. The VM runs in a detached `_vm-owner` process, so
`lnx` exits as soon as the guest command's status arrives. The owner keeps the
VM alive for an idle grace period (5s by default, `LNX_BROKER_IDLE_TTL_MS` to
override) so rapid-fire commands reuse the live VM without a restore; once
idle it asks the guest to quiesce, snapshots, and exits, so the next exec
restores systemd, the agent, and the rootfs from that point. A fresh boot
writes a full memory snapshot; restored runs use libkrun dirty tracking and
APFS clones to patch only changed RAM and disk blocks.

Per-run timings are appended to `~/.lnx/instances/<instance>/timings.log`.
Incremental snapshots skip `fsync` by default for speed; set
`KRUN_SNAPSHOT_SYNC=1` to make snapshot files crash-durable before returning.

Host shares always mount with virtio-fs DAX. The cache mode is recorded in
the snapshot compatibility stamp, so snapshots created under the removed
non-DAX mode refuse to memory-restore; clear them with
`lnx --instance <instance> snapshots clear`.

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
[upstream libkrun](https://github.com/containers/libkrun) does not have yet;
the intent is to upstream the snapshot work once it stabilizes. `CC_LINUX` is
needed at build time because libkrun compiles its own embedded Linux init
helper.
