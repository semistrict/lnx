#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="${LNX_KERNEL_VERSION:-7.1.2}"
major="${version%%.*}"
work="${LNX_KERNEL_WORKDIR:-$repo_root/target/kernel}"
downloads="$work/downloads"
archive="$downloads/linux-$version.tar.xz"
source_dir="$work/linux-$version"
out="${LNX_KERNEL_OUT:-$repo_root/target/vmlinuz-$version}"
url="${LNX_KERNEL_URL:-https://cdn.kernel.org/pub/linux/kernel/v$major.x/linux-$version.tar.xz}"

usage() {
  cat >&2 <<'USAGE'
usage: scripts/kernel-build.sh prepare|build-lnx|ensure-lnx|path|image-path

prepare   Download latest configured upstream source into target/kernel and apply kernel-patches.
build-lnx Build the patched kernel inside a normal lnx VM and write target/vmlinuz-<version>.
ensure-lnx
          Build inside lnx only when target/vmlinuz-<version> is missing or stale.
path      Print the patched source directory path.
image-path
          Print the patched kernel image output path.
USAGE
}

download_source() {
  mkdir -p "$downloads"
  if [[ -s "$archive" ]]; then
    return
  fi
  local partial="$archive.partial"
  rm -f "$partial"
  curl -fL "$url" -o "$partial"
  mv "$partial" "$archive"
}

prepare_source() {
  download_source
  rm -rf "$source_dir"
  mkdir -p "$work"
  tar -xJf "$archive" -C "$work"

  for patch_file in "$repo_root"/kernel-patches/*.patch; do
    [[ -e "$patch_file" ]] || continue
    patch -d "$source_dir" -p1 < "$patch_file"
  done

  cp "$repo_root/kernel.config" "$source_dir/.config"
  printf '%s\n' "$source_dir"
}

build_inside_lnx() {
  prepare_source >/dev/null

  local lnx_bin="${LNX_BIN:-$repo_root/target/debug/lnx}"
  if [[ ! -x "$lnx_bin" ]]; then
    (cd "$repo_root" && bun run build)
  fi

  local instance="${LNX_KERNEL_BUILD_INSTANCE:-kernel-build}"
  local memory_mib="${LNX_KERNEL_BUILD_MEMORY_MIB:-8192}"
  local cpus="${LNX_KERNEL_BUILD_CPUS:-4}"
  local jobs="${LNX_KERNEL_BUILD_JOBS:-}"
  local image="$source_dir/arch/arm64/boot/Image"

  "$lnx_bin" --instance "$instance" fs unshare --remove "$source_dir" >/dev/null 2>&1 || true
  "$lnx_bin" --instance "$instance" --root --memory-mib "$memory_mib" --cpus "$cpus" \
    -C "$source_dir" \
    env LNX_KERNEL_BUILD_JOBS="$jobs" bash -lc '
set -euo pipefail
apt-get update
DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
  build-essential \
  bc \
  bison \
  flex \
  dwarves \
  libelf-dev \
  libssl-dev \
  cpio \
  xz-utils \
  patch
make ARCH=arm64 olddefconfig
jobs="$LNX_KERNEL_BUILD_JOBS"
if [ -z "$jobs" ]; then
  jobs="$(nproc)"
fi
make ARCH=arm64 -j"$jobs" Image
'
  export_image_from_lnx "$lnx_bin" "$instance" "$image" "$out"
  printf '%s\n' "$out"
}

kernel_needs_build() {
  if [[ ! -s "$out" ]]; then
    return 0
  fi

  local input
  if [[ "$repo_root/kernel.config" -nt "$out" ]]; then
    return 0
  fi
  for input in "$repo_root"/kernel-patches/*.patch; do
    [[ -e "$input" ]] || continue
    if [[ "$input" -nt "$out" ]]; then
      return 0
    fi
  done

  return 1
}

ensure_inside_lnx() {
  if kernel_needs_build; then
    build_inside_lnx
    return
  fi
  printf '%s\n' "$out"
}

export_image_from_lnx() {
  local lnx_bin="$1"
  local instance="$2"
  local image="$3"
  local out="$4"
  local source="$image"

  if [[ ! -s "$source" ]]; then
    local state
    state="$("$lnx_bin" --instance "$instance" fs unshare "$image")"
    source="$(printf '%s\n' "$state" | awk -F': ' '/^upper: / { print $2; exit }')"
  fi
  if [[ -z "$source" || ! -s "$source" ]]; then
    printf 'kernel image was built but is not visible at %s\n' "$image" >&2
    printf 'inspect with: %s --instance %s fs unshare %s\n' "$lnx_bin" "$instance" "$image" >&2
    return 1
  fi

  mkdir -p "$(dirname "$out")"
  cp "$source" "$out"
  chmod 0644 "$out"
}

cmd="${1:-prepare}"
case "$cmd" in
  prepare)
    prepare_source
    ;;
  build-lnx)
    build_inside_lnx
    ;;
  ensure-lnx)
    ensure_inside_lnx
    ;;
  path)
    printf '%s\n' "$source_dir"
    ;;
  image-path)
    printf '%s\n' "$out"
    ;;
  -h|--help|help)
    usage
    ;;
  *)
    usage
    exit 2
    ;;
esac
