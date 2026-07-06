# lnx

Linux VMs on macOS that wake up with their memory, disk, and systemd state
intact.

```sh
lnx echo hello        # boots a full Linux VM, runs the command, exits
lnx apt-get install -y postgresql
lnx psql --version    # same machine, still installed
```

Between commands there is **no VM running**. When a command finishes and the
instance goes idle, `lnx` snapshots the VM — RAM, devices, disk — and exits.
The next command restores from that snapshot: systemd is already up, services
are still running, the page cache is still warm. Rapid-fire commands reuse the
live VM without a restore at all.

On an M5 Pro, `lnx /bin/true` completes in about **0.9s** when it restores a
4 GiB VM from a memory snapshot, and about **40ms** when the VM is still live
from a previous command.

Because instance state is just files (APFS clones for disk, snapshot files for
RAM), instances are cheap to **fork**: prepare one instance — repo checked
out, dependencies installed, server running — then stamp out copies of it, one
per experiment, test shard, or coding agent.

## What you get

- **A real Linux machine, not a container.** Ubuntu userland with systemd,
  its own kernel, apt, Docker-in-VM if you want it. Persistent per-instance
  rootfs.
- **Memory snapshots between commands.** Warm restores via libkrun dirty-page
  tracking and APFS clones — only changed RAM and disk blocks are written.
- **Instance forking and checkpoints.** `lnx fork` clones a prepared instance;
  checkpoints roll the filesystem back to a known-good state.
- **Host integration.** The current directory is shared into the guest
  (virtio-fs with DAX), host timezone forwarded, ports forwarded with
  `--forward`, and optional `https://p<port>-<instance>.lnx` URLs via ingress.
- **Nested KVM.** Pass `--nested-kvm` and run KVM workloads (including lnx
  itself) inside the guest.

Requirements: Apple Silicon Mac. The guest is arm64 Linux.

## Install

Download the latest release from
[GitHub Releases](https://github.com/semistrict/lnx/releases):

```sh
curl -LO https://github.com/semistrict/lnx/releases/latest/download/lnx-macos-arm64.tar.gz
tar -xzf lnx-macos-arm64.tar.gz
mv lnx ~/.local/bin/   # or anywhere on PATH
lnx echo hello         # downloads the kernel + rootfs image on first run
```

Or build from source:

```sh
brew install FiloSottile/musl-cross/musl-cross podman
git clone https://github.com/semistrict/lnx && cd lnx
bun run install        # builds, signs, installs to ~/.cargo/bin
lnx echo hello
```

Guest networking uses podman's `gvproxy` (`brew install podman`, or set
`GVPROXY_PATH`).

## Usage

```sh
lnx bash                          # interactive shell in the default instance
lnx --instance dev bash           # named instances are isolated machines
lnx --forward 8080:80 nginx       # forward Mac localhost:8080 to guest :80
lnx checkpoint -m "deps installed"
lnx fork dev2                     # clone the instance, disk and all
lnx instances list
lnx set cpus=4 memory-mib=8192    # persist per-instance settings
```

Optional HTTPS ingress — stable local URLs for every instance:

```sh
sudo lnx ingress enable
open https://p6080-default.lnx/
```

Ingress installs a `.lnx` resolver, loopback listeners, and a local CA that is
**name-constrained to `.lnx` hosts only** — it cannot sign certificates for
real domains. `lnx ingress disable` removes the CA from the keychain;
`sudo lnx ingress uninstall` removes every trace. Details in
[docs/security.md](docs/security.md).

## How it works

`lnx` boots a Linux kernel directly with [libkrun](https://github.com/containers/libkrun)
on Hypervisor.framework. A small static agent is injected via initramfs,
stages itself into the real ext4 rootfs, and hands off to systemd; commands
stream over vsock. A detached owner process holds the VM through an idle grace
period (default 5s), then quiesces the guest and snapshots. Snapshot restore
brings back the full machine state.

The snapshot/restore support is carried as patches on a copy of libkrun
vendored in-tree at `third_party/libkrun`; upstreaming is planned once the
interface stabilizes.

More in [docs/architecture.md](docs/architecture.md).

## Documentation

- [Architecture](docs/architecture.md)
- [Security notes](docs/security.md) — what ingress installs, the
  name-constrained CA, full uninstall
- [FAQ](docs/faq.md) — vs OrbStack/Lima/Apple `container`, vendored libkrun,
  platform support
- [Troubleshooting](docs/troubleshooting.md)
- [Testing](docs/testing.md)

## Building and developing

```sh
brew install FiloSottile/musl-cross/musl-cross podman llvm
bun run build          # debug build + codesign
bun run test           # Rust unit tests
bun run test:system    # core integration suite
```

The hypervisor entitlement requires every runnable binary to be codesigned;
the `bun run` scripts handle that. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Apache-2.0. Vendored third-party code retains its own notices; see
[NOTICE](NOTICE).
