# Contributing

Thanks for your interest in lnx!

## Prerequisites

- Apple Silicon Mac
- Rust (stable, 1.85+) with the `aarch64-unknown-linux-musl` target
- [Bun](https://bun.sh) for the build/test scripts
- `brew install FiloSottile/musl-cross/musl-cross podman llvm`

## Building

```sh
bun run build      # debug build + codesign
bun run release    # release build + codesign
```

Always build through the Bun scripts rather than raw `cargo build`: the
hypervisor entitlement requires the binary to be codesigned, and the scripts
handle that (an unsigned binary fails with `HV_DENIED`). They also build the
Linux helper binary used for nested runs.

## Testing

```sh
bun run test           # Rust unit tests
bun run test:system    # core integration suite
bun run test:full      # everything CI runs
```

See [docs/testing.md](docs/testing.md) for the full suite list and opt-in
tests. Guest kernel and rootfs images are downloaded automatically
(`lnx init --global`); building them from source is only needed when changing
`kernel.config`, `kernel-patches/`, or the Dockerfiles.

Tests must encode intended correct behavior — do not add tests that pass
because a known bug reproduces.

## Layout

- `src/` — host CLI and VM runner (Rust, libkrun)
- `guest-agent/` — static Linux agent, PID-1 staging and exec service
- `lnx-protocol/` — host/guest wire protocol
- `third_party/libkrun` — vendored libkrun fork with snapshot/restore support
- `scripts/test/` — integration suites (Bun/TypeScript)
- `docs/` — architecture, security, testing notes

## Pull requests

- Keep changes focused; include tests that prove the new behavior.
- `cargo fmt` before pushing; CI checks formatting.
- Licensing is Apache-2.0; contributions are accepted under the same terms.
