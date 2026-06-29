# Repository Guidance

Use `gh` for GitHub interactions.

Look in `~/src/` for reference repositories by default. Ask before cloning any
new repository for reference.

Do not edit generated files directly. Update the source inputs and run the
appropriate generator instead.

Tests must encode intended correct behavior. Do not add normal-suite tests that
pass because a known bug reproduces; keep temporary repros outside the passing
suite until their expectations are flipped to the fixed behavior.

## Signed Builds

Always build, test, and install local binaries through the signing-aware Bun
scripts. Use `bun run build`, `bun run release`, `bun run install`, and
`bun run test` instead of raw `cargo build`, `cargo install`, or unscripted
copies from `target/` when the resulting binary may run VMs or HVF
code paths. If a direct Cargo command is unavoidable for a narrow check, do not
treat its output as an installable or runnable VM binary until it has gone
through the repo signing step.

## Kernel Builds

The Depot kernel build is `.depot/workflows/kernel.yml`. It is manually
dispatched and runs:

```sh
depot build --project rc3tz55hnc --token "$DEPOT_TOKEN" --build-platform linux/arm64 --platform linux/arm64 -f Dockerfile.kernel -t lnx-kernel --load .
```

That job extracts `/build/arch/arm64/boot/Image` from the built image, writes it
as `vmlinuz`, compresses it to `vmlinuz.gz`, and uploads `vmlinuz.gz` as the
artifact.

For release image updates, `.depot/workflows/release-images.yml` has the same
kernel build in its `kernel` job and publishes the resulting `vmlinuz.gz` with
the rootfs artifact.

When changing VM kernel behavior, update `kernel.config`, `kernel-patches/`, or
`Dockerfile.kernel` as appropriate, then use the Depot artifact. The managed VM
kernel at `~/.lnx/vmlinuz` will not change until a new artifact is installed or
an explicit `--kernel` path is used.

## Instance Forks And Clones

When adding or changing any instance fork or clone behavior:

- All fork/clone entry points must use one central implementation for
  coordinating with source instances.
- That central implementation must detect a running source VM and ask it to
  produce a coherent checkpoint before cloning.
- Do not add command-specific live-file cloning paths; live instance files must
  not be cloned directly from disk.
- Preserve memory snapshots for memory-clone operations unless the command
  explicitly requests a cold rootfs-only copy.
- Never fall back from memory restore to cold boot without the user's explicit
  approval.
- Do not copy the shared kernel into project-local stores.
- Keep lnx fast first: do not add slower restore or snapshot paths unless the
  user explicitly asks for that tradeoff.

## macOS HVF Debugging

- `hv_vm_create` returning `-85377017` is `0xFAE94007`, which is `HV_DENIED`.
- Decode this by treating the negative return as a signed 32-bit value:
  `-85377017 & 0xffffffff = 0xFAE94007`. The low error value is `0x07`,
  matching Apple's `HV_DENIED`.
- Treat `HV_DENIED` as an entitlement/signing failure first, not as VM count or
  memory pressure.
- On macOS, do not run raw `cargo test --workspace` for HVF-capable tests. Use
  `bun run test`, or set
  `CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER="bun $PWD/scripts/test/codesign-runner.ts"`
  when invoking Cargo directly, so test binaries are signed with
  `com.apple.security.hypervisor`.
- If a workspace test fails with `VmCreate`/`HV_DENIED` but passes through the
  signing runner, treat the raw Cargo invocation as the problem rather than a
  code regression.
- If this appears after rebuilding or reinstalling the CLI, debug the installed
  binary's signature before chasing snapshot, ext4, or device-topology problems.
- After `cargo install`, verify the installed binary still has
  `com.apple.security.hypervisor`; `cargo install` can replace a signed binary
  with an ad-hoc/linker-signed binary that has no entitlements.
- Check with `codesign -d --entitlements :- ~/.cargo/bin/lnx`; fix with
  `codesign --entitlements entitlements.plist --force -s - ~/.cargo/bin/lnx`.
