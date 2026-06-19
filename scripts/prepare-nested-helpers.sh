#!/usr/bin/env bash
set -euo pipefail

profile="${1:-debug}"
case "$profile" in
  debug)
    release_arg=""
    ;;
  release)
    release_arg="--release"
    ;;
  *)
    echo "usage: $0 [debug|release]" >&2
    exit 2
    ;;
esac

linux_target="aarch64-unknown-linux-musl"
linker="${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER:-${CC_LINUX:-/opt/homebrew/bin/aarch64-linux-musl-gcc}}"
export CC_LINUX="${CC_LINUX:-$linker}"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$linker"

cargo_args=(build)
if [ -n "$release_arg" ]; then
  cargo_args+=("$release_arg")
fi
cargo_args+=(--target "$linux_target")
cargo "${cargo_args[@]}"

gvproxy="target/gvproxy-linux-arm64"
if [ ! -f "$gvproxy" ]; then
  curl -fL -o "$gvproxy" \
    https://github.com/containers/gvisor-tap-vsock/releases/download/v0.8.9/gvproxy-linux-arm64
fi
chmod +x "$gvproxy"
