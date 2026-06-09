#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/sign-notarize.sh [binary]

Environment:
  SIGN_IDENTITY       Required. Example: Developer ID Application: Name (TEAMID)
  NOTARY_PROFILE      Optional. Keychain profile made with notarytool store-credentials.
  APPLE_ID            Required if NOTARY_PROFILE is unset.
  APPLE_TEAM_ID       Required if NOTARY_PROFILE is unset.
  APPLE_PASSWORD      Required if NOTARY_PROFILE is unset. Use an app-specific password.
  ENTITLEMENTS        Optional. Defaults to entitlements.plist.
  OUT_DIR             Optional. Defaults to dist.

Examples:
  SIGN_IDENTITY="Developer ID Application: Name (TEAMID)" \
    NOTARY_PROFILE=lnx-notary \
    scripts/sign-notarize.sh target/release/lnx

  SIGN_IDENTITY="Developer ID Application: Name (TEAMID)" \
    APPLE_ID=you@example.com \
    APPLE_TEAM_ID=TEAMID \
    APPLE_PASSWORD=xxxx-xxxx-xxxx-xxxx \
    scripts/sign-notarize.sh target/release/lnx
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

binary="${1:-target/release/lnx}"
entitlements="${ENTITLEMENTS:-entitlements.plist}"
out_dir="${OUT_DIR:-dist}"

if [[ -z "${SIGN_IDENTITY:-}" ]]; then
  echo "error: SIGN_IDENTITY is required" >&2
  echo >&2
  usage >&2
  exit 2
fi

if [[ ! -f "$binary" ]]; then
  echo "error: binary not found: $binary" >&2
  exit 2
fi

if [[ ! -f "$entitlements" ]]; then
  echo "error: entitlements file not found: $entitlements" >&2
  exit 2
fi

mkdir -p "$out_dir"

echo "signing $binary"
codesign \
  --force \
  --options runtime \
  --timestamp \
  --entitlements "$entitlements" \
  -s "$SIGN_IDENTITY" \
  "$binary"

echo "verifying signature"
codesign --verify --strict --verbose=2 "$binary"

archive="$out_dir/lnx-notarize.zip"
rm -f "$archive"
echo "creating $archive"
ditto -c -k --keepParent "$binary" "$archive"

echo "submitting to Apple notarization"
if [[ -n "${NOTARY_PROFILE:-}" ]]; then
  xcrun notarytool submit "$archive" --keychain-profile "$NOTARY_PROFILE" --wait
else
  if [[ -z "${APPLE_ID:-}" || -z "${APPLE_TEAM_ID:-}" || -z "${APPLE_PASSWORD:-}" ]]; then
    echo "error: set NOTARY_PROFILE, or set APPLE_ID, APPLE_TEAM_ID, and APPLE_PASSWORD" >&2
    exit 2
  fi
  xcrun notarytool submit "$archive" \
    --apple-id "$APPLE_ID" \
    --team-id "$APPLE_TEAM_ID" \
    --password "$APPLE_PASSWORD" \
    --wait
fi

echo "assessing notarized binary"
spctl --assess --type execute --verbose "$binary"

echo "done: $binary"
