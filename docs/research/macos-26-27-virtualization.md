# macOS 26/27 virtualization changes relevant to lnx

Researched 2026-10-03 on an M5 Pro running macOS 27.0.1 (26A434), against the
macOS 27.0 SDK (Xcode) and the 26.5 Command Line Tools SDK. Items marked
**[probed]** were verified on that host with small signed C programs.

## New APIs

| API / feature | Min macOS | Relevance | Notes |
|---|---|---|---|
| `hv_vm_config_set_ipa_granule` (`hv_vm_config.h`) | 26.0 | Medium | Default stage-2 granule is 16K. With 4K, `hv_vm_protect`/`hv_vm_map` accept 4K-aligned ranges **[probed]**, enabling finer dirty tracking than today's 2 MiB blocks and 4K-aligned DAX/pmem windows. Costs more stage-2 faults and reportedly shrinks usable IPA space. Works together with EL2 **[probed]**. |
| vmnet "network" API: `vmnet_network_configuration_create`, `vmnet_network_create`, `vmnet_interface_start_with_network`, port forwarding, DHCP reservations, `vmnet_network_copy_serialization` | 26.0 | **High** | Works unprivileged when the binary is ad-hoc signed with `com.apple.security.virtualization` **[probed]**; with only `com.apple.security.hypervisor` it fails with `VMNET_MEM_FAILURE`. Could replace gvproxy entirely, including port forwarding; serialization lets forks share a subnet. |
| `vmnet_enable_virtio_header_key` | 15.4 | High (with above) | virtio-net header passthrough (TSO/checksum offload). |
| `vmnet_read_max_packets_key` / `vmnet_write_max_packets_key` | 15.0 | Medium | Batched packet I/O. |
| `hv_vcpu_get_wait_for_interrupt_time` | 27.0 | Medium | Cumulative WFI time; requires the HVF GIC. |
| `hv_vcpu_get_serror` / `hv_vcpu_set_serror` | 27.0 | Medium | Pending SError state: should be part of vCPU snapshot state. |
| `hv_vcpu_invalidate_tlb` | 27.0 | Medium | Host-initiated EL1 TLBI; useful after rewriting guest memory under a live vCPU (in-place restore, fork) **[probed]**. |
| New ID registers `ID_AA64ISAR2/PFR2/MMFR3/MMFR4` | 27.0 | **High** for cross-host restore | Record in snapshots and check/mask for portability. |
| EL2, HVF GIC, opaque GIC state (`hv_gic_state_*`, `hv_gic_set_state`) | 15.0 | in use | On M5 Pro: EL2 supported, max IPA 40 bits, default 36, max 64 vCPUs **[probed]**. |
| SME state APIs | 15.2 | Low–Medium | Needed to snapshot SME-using guests. |
| `F_RDADVISEV` (vectored readahead) | 27 SDK | Medium | Prefetch `pages.img` ranges on restore. |
| VZ custom virtio devices, DiskImageKit/ASIF, USB passthrough | 26/27 | Low | Virtualization.framework only. |
| `clonefile` | unchanged | — | No APFS clone API changes 26.5 → 27. |

## Behaviour changes / risks

1. **WFI no longer exits to userspace when the HVF GIC is present** **[probed on 27.0.1]**.
   With `hv_gic_create`, the vCPU blocks in the kernel until kicked. libkrun's
   `EC_WFX_TRAP` handling (`third_party/libkrun/src/hvf/src/lib.rs`), including
   the deterministic-time jump, therefore never runs on the default HVF GIC path.
   Not verified whether macOS 26 behaves the same.
2. **`hv_vcpu_set_pending_interrupt` returns `HV_UNSUPPORTED` when a GIC exists**
   **[probed; documented in the 27 header]**. Any wake path that relies on it
   (e.g. the RebaseTimer wake thread in `vmm/src/macos/vstate.rs`) is a no-op.
3. **`hv_gic_set_state` can fail after an OS update** (header comment). Snapshots
   need a host/OS compatibility record and must fail explicitly (no silent
   cold-boot fallback, per AGENTS.md).
4. **Nested virt caveats**: QEMU reports `VTIMER_ACTIVATED` does not fire while a
   nested guest runs at EL2, EL2+SME unsupported. Always gate EL2 on
   `hv_vm_config_get_el2_supported`.
5. **vmnet gotchas**: networks lose DHCP reservations/port forwards when the last
   interface stops (keep an anchor interface); Local Network privacy applies to
   host→vmnet traffic; `pfd` InternetSharing hang on 27.0 (apple/container#2275).

## Recommendations (not yet implemented)

1. In-process vmnet networking (`com.apple.security.virtualization` entitlement),
   replacing embedded gvproxy; native port forwarding; static per-VM IPs.
2. Make the HVF-GIC path independent of WFI exits and `set_pending_interrupt`;
   re-check deterministic time; add a regression test.
3. Snapshot hardening: record host OS build + all `ID_AA64*` registers, capture
   SError state, invalidate TLB after in-place memory restore.
4. Benchmark the 4K IPA granule for finer dirty tracking (opt-in).
5. `F_RDADVISEV` restore prefetch.
