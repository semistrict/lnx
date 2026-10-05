# Release automation

Pushes to `main` and pull requests run `.github/workflows/release-binary.yml`,
which builds an ad-hoc signed binary and keeps it as a workflow artifact.

Pushing a version tag such as `v0.3.1` runs that workflow's release job. The
tag must match the `version` in `Cargo.toml` and point at the current `main`
commit. The protected `release` environment supplies the credentials to sign
with Developer ID, notarize, attest provenance, and publish
`lnx-macos-arm64.tar.gz`; the job then points `Formula/lnx.rb` at the new
release. If any required secret is absent, the job exits successfully without
publishing anything.

The tarball holds `lnx` (arm64, hardened runtime, hypervisor entitlement,
notarized) and `lnx-linux-aarch64`, the Linux helper for nested runs. A bare
executable cannot be stapled, so Gatekeeper fetches its notarization ticket
online the first time it runs.

## Configure GitHub

Create an environment named `release`, restrict it to protected tags, and add a
required reviewer. Store these environment secrets (the same ones Pablo uses):

- `DEVELOPER_ID_CERTIFICATE_P12_BASE64` — a base64-encoded export of the
  Developer ID Application certificate and its private key.
- `DEVELOPER_ID_CERTIFICATE_PASSWORD` — the export password for that P12.
- `NOTARY_API_KEY_P8_BASE64` — a base64-encoded App Store Connect team API key.
- `NOTARY_API_KEY_ID` — the API key identifier.
- `NOTARY_API_ISSUER_ID` — the team API issuer identifier.

Use a team API key; Apple does not allow individual API keys with
`notarytool`. Keep the certificate and key files out of the repository.

The environment can be created from the repository checkout with:

```sh
gh api --method PUT repos/{owner}/{repo}/environments/release
```

Add its secrets without putting their values in shell history:

```sh
gh secret set --env release DEVELOPER_ID_CERTIFICATE_P12_BASE64
gh secret set --env release DEVELOPER_ID_CERTIFICATE_PASSWORD
gh secret set --env release NOTARY_API_KEY_P8_BASE64
gh secret set --env release NOTARY_API_KEY_ID
gh secret set --env release NOTARY_API_ISSUER_ID
```

Configure the required reviewer and protected-tag rule in **Settings →
Environments → release**.

## Build a release locally

`scripts/build-distribution.sh` (`bun run dist`) is what the release job runs.
It needs a Developer ID Application identity in the keychain and a
`notarytool` keychain profile, by default `lnx-notary`:

```sh
xcrun notarytool store-credentials lnx-notary \
  --key AuthKey_XXXXXXXXXX.p8 --key-id XXXXXXXXXX --issuer <issuer-uuid>
```

`LNX_NOTARY_PROFILE` selects another profile (for example `pablo-notary`, which
uses the same team), `LNX_NOTARY_KEYCHAIN` another keychain, and
`LNX_SIGNING_IDENTITY` an exact signing identity. The script prints the path of
`dist/lnx-macos-arm64.tar.gz`, next to its `.sha256`.

## Publish

Update `version` in `Cargo.toml`, `lnx-protocol/Cargo.toml`, and
`guest-agent/Cargo.toml`, commit and push the change to `main`, then create the
matching tag:

```sh
git tag v0.3.1
git push origin v0.3.1
```

## Image releases

The kernel, rootfs, and nested helper that lnx downloads come from an
`images-v*` release (`.github/workflows/release-images.yml`). The binary pins
their SHA-256 digests in `src/release_assets.json` and refuses a download that
does not match. After publishing a new image release, pin it:

```sh
bun run images:pin images-v0.7.0
```
