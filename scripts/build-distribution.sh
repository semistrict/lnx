#!/bin/zsh
# Builds the release tarball: an arm64 lnx signed with Developer ID
# (hardened runtime, hypervisor entitlement) and notarized by Apple, plus
# the Linux helper for nested runs. Prints the tarball's path.
#
#   scripts/build-distribution.sh
#
# Environment:
#   LNX_SIGNING_IDENTITY  Signing identity; defaults to the first
#                         "Developer ID Application" identity in the keychain.
#   LNX_NOTARY_PROFILE    notarytool keychain profile, made with
#                         `xcrun notarytool store-credentials`; defaults to
#                         pablo-notary, the same team's profile Pablo uses.
#   LNX_NOTARY_KEYCHAIN   Keychain holding that profile, when not the default.
#
# A bare executable cannot be stapled; Gatekeeper fetches its notarization
# ticket online the first time it runs.

set -euo pipefail

script_directory=${0:A:h}
project_directory=${script_directory:h}
cd "$project_directory"

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
binary="$project_directory/target/release/lnx"
linux_helper="$project_directory/target/aarch64-unknown-linux-musl/release/lnx"
dist_directory="$project_directory/dist"
release_tarball="$dist_directory/lnx-macos-arm64.tar.gz"
temporary_directory=$(mktemp -d "${TMPDIR:-/tmp}/lnx-notarization.XXXXXX")
submission_zip="$temporary_directory/lnx.zip"
trap 'rm -rf "$temporary_directory"' EXIT

signing_identity=${LNX_SIGNING_IDENTITY:-$(security find-identity -v -p codesigning \
    | sed -n 's/.*"\(Developer ID Application:[^"]*\)".*/\1/p' \
    | head -n 1)}
if [[ -z $signing_identity ]]; then
    echo "A Developer ID Application signing identity is required." >&2
    exit 1
fi

echo "building lnx $version" >&2
CC_LINUX=${CC_LINUX:-/opt/homebrew/bin/aarch64-linux-musl-gcc} cargo build --release >&2
"$script_directory/prepare-nested-helpers.sh" release >&2
test "$(lipo -archs "$binary")" = arm64

echo "signing with $signing_identity" >&2
codesign --force --options runtime --timestamp \
    --entitlements "$project_directory/entitlements.plist" \
    --sign "$signing_identity" \
    "$binary"
codesign --verify --strict --verbose=2 "$binary"
codesign --display --entitlements - "$binary" 2>/dev/null | grep -q com.apple.security.hypervisor

ditto -c -k --keepParent "$binary" "$submission_zip"
notary_arguments=(--keychain-profile "${LNX_NOTARY_PROFILE:-pablo-notary}")
if [[ -n ${LNX_NOTARY_KEYCHAIN:-} ]]; then
    notary_arguments+=(--keychain "$LNX_NOTARY_KEYCHAIN")
fi
echo "notarizing" >&2
submission=$(xcrun notarytool submit "$submission_zip" "${notary_arguments[@]}" \
    --wait --output-format json)
submission_id=$(plutil -extract id raw - <<<"$submission")
submission_status=$(plutil -extract status raw - <<<"$submission")
if [[ $submission_status != Accepted ]]; then
    echo "notarization $submission_id finished as $submission_status" >&2
    xcrun notarytool log "$submission_id" "${notary_arguments[@]}" >&2 || true
    exit 1
fi
echo "notarization $submission_id accepted" >&2
spctl --assess --type install --verbose=4 "$binary" >&2

mkdir -p "$dist_directory"
cp "$binary" "$temporary_directory/lnx"
cp "$linux_helper" "$temporary_directory/lnx-linux-aarch64"
tar -C "$temporary_directory" -czf "$release_tarball" lnx lnx-linux-aarch64
(cd "$dist_directory" && shasum -a 256 lnx-macos-arm64.tar.gz > lnx-macos-arm64.tar.gz.sha256)

echo "$release_tarball"
