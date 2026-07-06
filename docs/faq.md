# FAQ

## How is this different from OrbStack, Lima, colima, or Apple's `container`?

Those tools all keep a Linux VM (or several) running and give you fast access
to it. `lnx` is built around a different primitive: **the VM's memory and disk
state are a snapshot on disk**. When no command is running, there is no VM —
the next command restores systemd, running services, and all, from the last
snapshot. The rootfs is pmem/DAX-mapped, so guest file data needs no page
cache of its own — snapshots stay small and restores stay fast. That enables
things the others don't do:

- **No idle cost.** Nothing runs between commands; state still feels warm.
- **Fork.** `lnx fork` clones an instance — including its disk — using APFS
  clones, so fan-out is cheap. Prepare one instance (checkout, deps installed,
  server running), then fork it per experiment or per agent.
- **Checkpoints.** Roll an instance's filesystem back to a known-good point.

If you want a always-on Docker replacement, OrbStack is great. If you want
disposable-but-stateful Linux environments that appear on demand, that's
`lnx`.

## Why is libkrun vendored?

Upstream [libkrun](https://github.com/containers/libkrun) has no memory
snapshot/restore. The copy vendored in-tree at `third_party/libkrun` adds
snapshot capture/restore with dirty-page tracking on macOS/HVF. The plan is to
upstream it once the interface stabilizes. Everything needed to build `lnx`
lives in this repository.

## Does it run on Intel Macs?

No. `lnx` is Apple Silicon (arm64) only. The guest is arm64 Linux.

## Does it run on Linux?

The core exec path also works on Linux hosts with KVM (that is how the nested
test suite runs), but macOS is the primary target and the Linux snapshot path
currently captures full RAM rather than incremental dirty pages.

## What about x86 binaries inside the guest?

The guest is arm64. Use Rosetta-free options like qemu-user or arm64 builds.
Running the guest itself under emulation is out of scope.

## Is the name related to the lnx search engine?

No relation. This `lnx` is a Linux VM runner; the name is just "Linux" minus
two vowels.

## Why does ingress install a trusted CA?

So `https://p<port>-<instance>.lnx` URLs work without per-site warnings. The
CA is name-constrained to `.lnx` hosts only, `lnx ingress disable` removes it
from the keychain, and `sudo lnx ingress uninstall` deletes its on-disk state
too — see [security.md](security.md). Ingress is entirely optional; port
forwarding with `--forward` works without it.

## Where does my data live?

Everything is under `~/.lnx`: instance rootfs images, memory snapshots,
checkpoints, logs, and ingress state. Delete an instance directory (or all of
`~/.lnx`) and it is gone.
