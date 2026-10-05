# Releases

Releases are made from a Mac, not CI. From a clean `main` that matches
`origin/main`:

```sh
scripts/release.sh 0.3.1
```

It bumps the version, builds the binary signed with Developer ID and notarized
by Apple (`scripts/build-distribution.sh`), points `Formula/lnx.rb` at it,
commits, tags `v0.3.1`, pushes, and creates the GitHub release with
`lnx-macos-arm64.tar.gz` and its checksum.

It needs a "Developer ID Application" identity in the keychain and a
`notarytool` keychain profile, by default `pablo-notary` (set
`LNX_NOTARY_PROFILE` to use another). To build and notarize without
publishing, run `scripts/build-distribution.sh`.

A bare executable cannot be stapled, so Gatekeeper fetches its notarization
ticket online the first time it runs.

## Image assets

The kernel, rootfs, and nested helper that lnx downloads come from an
`images-v*` release. Their SHA-256 digests are compiled into the binary from
`src/release_assets.json`, and lnx refuses a download that does not match.
After publishing a new image release, pin it:

```sh
bun run images:pin images-v0.7.0
```
