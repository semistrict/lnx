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

## "The saved memory snapshot cannot be resumed"

An instance resumes its saved memory only in the shape it was taken with
(CPUs, memory, host shares, nested virtualization) and with a guest agent
whose protocol this lnx still speaks (its own, or an older one it keeps
supporting). The error says which of those differs.
Dropping the saved memory keeps the disk; the next run boots from it:

```sh
lnx --instance <instance> snapshots clear
```

`lnx set` never causes this: new settings apply at the next cold boot.

## "stopped unexpectedly after running commands"

The VM died after it had served commands, so its disk may hold writes a
command reported as done that were never saved. lnx will not guess; choose:

```sh
lnx --instance <instance> recover --keep      # keep its disk, lose its memory
lnx --instance <instance> recover --discard   # go back to the last saved state
```

## Undoing a restore

`lnx restore CHECKPOINT` keeps the state it replaces as the checkpoint
`before-restore`:

```sh
lnx --instance <instance> restore before-restore
```

## lnx exit statuses

`lnx` exits with the guest command's status. Failures in lnx itself exit
125, `--timeout` exits 124, and a client stopped by a signal exits 128 plus
the signal number.

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
