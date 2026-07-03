# lnx

`lnx` is a Rust/libkrun Linux VM runner for macOS. It boots a Linux kernel
directly, uses a normal systemd rootfs, and preserves VM memory plus disk state
with libkrun snapshots between commands.

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

Basic exec flow:

1. The Rust build script compiles `guest-agent/src/main.rs` into a static Linux
   binary named `lnx-agent`.
2. Before boot, the host writes an initramfs containing that binary.
3. The binary's `--init` mode stages itself into `/usr/local/lib/lnx` in the
   real root.
4. systemd starts `lnx-agent --agent 10240`.
5. The host connects to `lnx-agent` over libkrun's vsock-to-Unix-socket port
   mapping, sends one argv vector, streams stdout/stderr frames, and exits with
   the guest command status.

Build requirements on macOS:

```sh
brew install FiloSottile/musl-cross/musl-cross
brew install podman
CC_LINUX=/opt/homebrew/bin/aarch64-linux-musl-gcc cargo build
codesign --entitlements entitlements.plist --force -s - target/debug/lnx
target/debug/lnx /bin/echo hello
```

`lnx` builds against the `wip/snapshot-restore-20260525-0606` branch of
`https://github.com/semistrict/libkrun`. `CC_LINUX` is needed because libkrun
compiles its own embedded Linux init helper.

Networking uses podman's `gvproxy` via libkrun's `krun_add_net_unixgram`
backend. The default path is `/opt/homebrew/opt/podman/libexec/podman/gvproxy`;
set `GVPROXY_PATH` if it lives somewhere else.

Ingress:

```sh
sudo lnx ingress enable
open https://p6080.default.lnx/
```

`ingress enable` installs the `.lnx` resolver, starts local HTTP and HTTPS
listeners, and trusts a local `lnx` CA in the macOS System keychain. HTTPS
certificates are generated per `.lnx` host on first use and terminate at the
host ingress before proxying plain HTTP/WebSocket traffic to the guest port.

Memory snapshot restore defaults to `~/.lnx/instances/<instance>/memory-snapshots/latest`
and can be overridden with `--snapshot <dir>`. The VM runs in a detached
`_vm-owner` process, so `lnx` exits as soon as the guest command's status
arrives. The owner keeps the VM alive for an idle grace period (5s by default,
`LNX_BROKER_IDLE_TTL_MS` to override) so rapid-fire commands reuse the live VM
without a restore; once idle it asks the guest to quiesce, snapshots, and
exits, so the next exec restores systemd, the agent, and the rootfs from that
point. A fresh boot writes a full memory snapshot; restored runs use libkrun
dirty tracking and APFS clones to patch only changed RAM and disk blocks.

Per-run timings are appended to `~/.lnx/instances/<instance>/timings.log`.
Incremental snapshots skip `fsync` by default for speed; set
`KRUN_SNAPSHOT_SYNC=1` to make snapshot files crash-durable before returning.

Host shares always mount with virtio-fs DAX. The cache mode is recorded in
the snapshot compatibility stamp, so snapshots created under the removed
non-DAX mode refuse to memory-restore; clear them with
`lnx --instance <instance> snapshots clear`.

Packages:

`lnx` keeps a per-user Nix package store shared by every instance and every
project (under `LNX_BASE` when set, otherwise `~/.lnx/stores/nix-linux-<arch>`;
on macOS it is a case-sensitive APFS sparsebundle). Packages are built inside a
throwaway `nixos/nix` builder VM and the resulting store is mounted read-only
into guests, where the profile's binaries are linked into `/usr/local/bin`.

```sh
lnx packages install ripgrep            # bare names mean nixpkgs#<name>
lnx packages install nixpkgs#go --bin go  # --bin asserts the profile provides it
lnx packages list
lnx packages gc                         # drop store paths outside the profile closure
lnx packages paths
```

The first run of any instance installs a default Node.js toolchain
(node/npm/npx/pnpm). Skip that with `--package-store disabled` (which also
skips mounting the store for that run) or by setting
`LNX_SKIP_DEFAULT_PACKAGES=1`. The mounted store is part of the snapshot
compatibility stamp: when the store appears, disappears, or moves, the next
run cold-boots instead of restoring the latest memory snapshot.

Nested KVM testing:

```sh
bun run test:nested-kvm
```

The nested test compiles `lnx` for `aarch64-unknown-linux-musl`, boots an outer
`lnx --nested-kvm` guest, verifies that an inner `lnx` VM can boot after the
outer VM has gone through `lnxctl snapshot-exit`, then runs the Linux-host
compatible part of the integration suite inside the nested-capable guest.

Current caveats:

- Inner nested `lnx` runs use `LNX_ROOTFS_BACKEND=block`; pmem/DAX rootfs inside
  the nested Linux host still hits KVM mapping limitations.
- Linux libkrun snapshot APIs are wired for a full-RAM KVM/aarch64 capture and
  restore path. Incremental dirty-log snapshots are not implemented yet, so the
  Linux path is expected to be correct but heavier than the macOS/HVF path until
  it grows KVM dirty-log support.
- `system` and `stress` have nested-safe coverage for their non-snapshot
  behavior; their snapshot-specific assertions should move into the nested
  Linux suite after the Linux full-RAM restore path has end-to-end runtime
  coverage.
- Linux virtiofs write allowlist enforcement is not active today, so the
  policy-specific virtiofs restore/fork checks do not run inside the nested
  Linux host.
- `stock-ubuntu` remains excluded: `snapd` panics while parsing the nested guest
  kernel command line under nested KVM, and a stock boot/apt probe hung instead
  of producing bounded signal.
- Browser snapshot coverage remains opt-in and snapshot/fork-dependent.
- Ingress and privileged ingress tests are macOS host tests because they depend
  on launchd, `/etc/resolver`, keychain/sudo setup, and privileged host ports.
