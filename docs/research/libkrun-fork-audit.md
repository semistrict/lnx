# libkrun fork audit (2026-10-03)

Scope: `third_party/libkrun` on `main` @ 3bbcb655. Fork base is upstream
`2c6b175` (2026-06-29); upstream `main` was 190 commits ahead at audit time.
Against the base, the fork changes 107 files (+23k/−3.8k).

## What the fork adds

- **HVF snapshot/restore** (`src/hvf/src/state.rs`, `src/vmm/src/macos/snapshot/*`):
  - Captured state: general, FP/SIMD and EL1/EL2 system registers, per-vCPU GIC
    registers, and the vtimer offset.
  - The HVF GIC is saved as an opaque state blob.
  - RAM is captured as a clone-and-patch `pages.img`.
  - Dirty tracking uses write protection at 2 MiB-block granularity
    (`hv_vm_protect`).
  - Snapshots can be restored across hypervisors in both directions (KVM ↔ HVF).
- **Device pause/serialize/restore** for block, net, fs, vsock, console, rng,
  balloon, pmem and vhost-user.
  - virtio-pmem and DAX mappings in virtio-fs.
  - Write allowlists for virtio-fs.
  - Host-side writes to guest memory are recorded as explicit dirty marks.
- **Rust API**: a native builder plus `VmHandle::snapshot` replaces the C API.
- **Linux/KVM**: full-RAM snapshots.

Upstream has since added:
- live pause/resume with vtimer rebase;
- a Rust v2 API;
- max IPA size support;
- a `select!`-based pause fix.

Upstream still has no snapshotting, GIC state, dirty tracking or pmem.

## Ranked risks

1. **A failed capture silently corrupts the next incremental snapshot.** Taking
   the dirty set clears the bitmap. If the capture then fails, those bits are
   gone, and the next incremental snapshot patches a stale `pages.img`.
2. **The incremental base is not checked against the VM's lineage.** Any
   `pages.img` already in the target directory is used as the base.
3. **vhost-user-fs writes to guest RAM bypass dirty tracking.**
4. **Snapshot vs virtio-fs DAX can deadlock.** The snapshot holds the Vmm mutex
   while joining the fs worker, and that worker is waiting on the vmm worker,
   which needs the same mutex.
5. **Pausing can hang on idle vCPUs.** A vCPU blocked on the WFE channel never
   sees the Pause event. Upstream has fixed this.
6. **The pause/resume channels desync after a timeout.** Stale replies stay
   queued and are read by the next operation.
7. **ID registers and MIDR are captured but never validated on restore.** This
   matters for cross-host restore.
8. **`state.rs` swallows save/restore errors.** A partial snapshot can be
   published.
9. **The restore timer rebase calls HVF from threads that don't own the vCPU.**
   A detached thread calls `hv_vcpu_set_vtimer_mask` and
   `hv_vcpu_set_pending_interrupt`. On macOS 27 the latter is also unsupported
   when a GIC exists (see `macos-26-27-virtualization.md`).
10. **Panics:**
    - a snapshot racing guest shutdown panics;
    - `wake_all` sends unbounded wakeups and calls `hv_vcpus_exit().unwrap()`.

Other findings:
- Durability is off by default (`KRUN_SNAPSHOT_SYNC`).
- Publish deletes the old directory before renaming the new one into place.
- `#[serde(default)]` has no effect under bincode, so adding fields breaks old
  snapshots.
- The binary hard-links macOS 15 HVF GIC symbols, so the `libloading` fallback
  is moot.
