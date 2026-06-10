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

Memory snapshot restore defaults to `~/.lnx/images/<instance>/memory-snapshots/latest`
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
