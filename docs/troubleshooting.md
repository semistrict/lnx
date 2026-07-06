# Troubleshooting

## `HV_DENIED` or the VM refuses to start after a rebuild

The binary must be codesigned with the hypervisor entitlement. A raw
`cargo build` produces an unsigned binary; always build through the repo
scripts, which sign automatically:

```sh
bun run build      # debug
bun run release    # release
```

or sign manually:

```sh
codesign --entitlements entitlements.plist --force -s - target/debug/lnx
```

## Snapshot refuses to restore after upgrading lnx

Snapshots carry a compatibility stamp. When the VM configuration changes
incompatibly, clear the instance's snapshots and let the next run boot fresh:

```sh
lnx --instance <instance> snapshots clear
```

## Downloaded release binary is blocked by Gatekeeper

If you downloaded the tarball with a browser, macOS may quarantine it:

```sh
xattr -d com.apple.quarantine lnx
```

`curl`/`tar` downloads are not quarantined.

## Build fails with `libclang.dylib` not found

libkrun's bindgen build needs LLVM's libclang:

```sh
brew install llvm
export LIBCLANG_PATH=/opt/homebrew/opt/llvm/lib
```

(The `bun run` scripts set this automatically for tests.)

## Where to look

- Per-run timing traces: `~/.lnx/instances/<instance>/timings.log`
- Instance logs: `lnx logs`
- Instance state and configuration: `lnx inspect`
