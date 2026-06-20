# Repository Guidance

Use `gh` for GitHub interactions.

Look in `~/src/` for reference repositories by default. Ask before cloning any
new repository for reference.

Do not edit generated files directly. Update the source inputs and run the
appropriate generator instead.

Tests must encode intended correct behavior. Do not add normal-suite tests that
pass because a known bug reproduces; keep temporary repros outside the passing
suite until their expectations are flipped to the fixed behavior.

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
