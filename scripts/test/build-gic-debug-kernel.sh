#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
out="${LNX_GIC_DEBUG_KERNEL_OUT:-$repo_root/target/gic-debug-vmlinuz}"
image="${LNX_GIC_DEBUG_KERNEL_IMAGE:-lnx-gic-debug-kernel}"
container="${image}-extract"
work="$repo_root/target/gic-debug-kernel"
dockerfile="$work/Dockerfile"
source_dir="${LNX_GIC_DEBUG_KERNEL_SOURCE:-}"
cross_compile="${LNX_GIC_DEBUG_CROSS_COMPILE:-aarch64-linux-musl-}"

if [[ -z "$source_dir" && -d "$HOME/src/linux" ]]; then
  source_dir="$HOME/src/linux"
fi

mkdir -p "$work" "$(dirname "$out")"

cat >"$dockerfile" <<'DOCKERFILE'
FROM ubuntu:26.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    bc \
    bison \
    flex \
    dwarves \
    libelf-dev \
    libssl-dev \
    linux-source-7.0.0 \
    cpio \
    patch \
    && rm -rf /var/lib/apt/lists/*

RUN mkdir /build && \
    tar xf /usr/src/linux-source-7.0.0.tar.bz2 -C /build --strip-components=1

WORKDIR /build

COPY kernel-patches /kernel-patches
RUN for patch in /kernel-patches/*.patch; do patch -p1 < "$patch"; done

COPY scripts/test/kernel-*-debug.patch /tmp/
RUN for patch in /tmp/kernel-*-debug.patch; do patch -p1 < "$patch"; done

COPY kernel.config .config
RUN make olddefconfig && make -j$(nproc)
DOCKERFILE

engine="${LNX_GIC_DEBUG_CONTAINER_ENGINE:-}"
if [[ -z "$engine" ]]; then
  if [[ -n "$source_dir" ]] && command -v "${cross_compile}gcc" >/dev/null 2>&1; then
    engine=local
  elif command -v docker >/dev/null 2>&1 && docker version >/dev/null 2>&1; then
    engine=docker
  elif command -v podman >/dev/null 2>&1 && podman info >/dev/null 2>&1; then
    engine=podman
  elif [[ -x "$repo_root/target/debug/lnx" ]]; then
    engine=lnx
  else
    echo "missing builder: start Docker/Podman, run bun run build for lnx, or set LNX_GIC_DEBUG_CONTAINER_ENGINE" >&2
    exit 1
  fi
fi

case "$engine" in
  local)
    if [[ -z "$source_dir" ]]; then
      echo "local debug kernel build requires LNX_GIC_DEBUG_KERNEL_SOURCE or ~/src/linux" >&2
      exit 1
    fi
    make_cmd="${LNX_GIC_DEBUG_MAKE:-}"
    if [[ -z "$make_cmd" ]]; then
      if command -v gmake >/dev/null 2>&1; then
        make_cmd=gmake
      else
        make_cmd=make
      fi
    fi
    jobs="${LNX_GIC_DEBUG_KERNEL_JOBS:-}"
    if [[ -z "$jobs" ]]; then
      jobs="$(sysctl -n hw.ncpu 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"
    fi
    source_work="$work/source"
    build_dir="$work/build"
    make_extra=()
    if [[ -n "${LNX_GIC_DEBUG_HOSTCFLAGS:-}" ]]; then
      make_extra+=("HOSTCFLAGS=${LNX_GIC_DEBUG_HOSTCFLAGS}")
    fi
    rm -rf "$source_work" "$build_dir"
    mkdir -p "$source_work" "$build_dir" "$(dirname "$out")"
    cp -a "$source_dir"/. "$source_work"/
    if [[ -z "${LNX_GIC_DEBUG_SKIP_KERNEL_PATCHES:-}" ]]; then
      for patch_file in "$repo_root"/kernel-patches/*.patch; do
        patch -d "$source_work" -p1 < "$patch_file"
      done
    fi
    for patch_file in "$repo_root"/scripts/test/kernel-*-debug.patch; do
      patch -d "$source_work" -p1 < "$patch_file"
    done
    cp "$repo_root/kernel.config" "$build_dir/.config"
    "$make_cmd" -C "$source_work" O="$build_dir" ARCH=arm64 CROSS_COMPILE="$cross_compile" "${make_extra[@]}" olddefconfig
    "$make_cmd" -C "$source_work" O="$build_dir" ARCH=arm64 CROSS_COMPILE="$cross_compile" "${make_extra[@]}" -j"$jobs" Image
    cp "$build_dir/arch/arm64/boot/Image" "$out"
    printf '%s\n' "$out"
    exit 0
    ;;
  docker)
    docker buildx build --platform linux/arm64 -f "$dockerfile" -t "$image" --load "$repo_root"
    ;;
  podman)
    podman build --platform linux/arm64 -f "$dockerfile" -t "$image" "$repo_root"
    ;;
  lnx)
    lnx_bin="${LNX_BIN:-$repo_root/target/debug/lnx}"
    memory_mib="${LNX_GIC_DEBUG_LNX_MEMORY_MIB:-8192}"
    cpus="${LNX_GIC_DEBUG_LNX_CPUS:-4}"
    instance="${LNX_GIC_DEBUG_LNX_INSTANCE:-gic-debug-kernel}"
    "$lnx_bin" --instance "$instance" --root --memory-mib "$memory_mib" --cpus "$cpus" env \
      LNX_REPO_ROOT="$repo_root" \
      LNX_KERNEL_OUT="$out" \
      LNX_GIC_DEBUG_KERNEL_SOURCE="$source_dir" \
      LNX_GIC_DEBUG_KERNEL_JOBS="${LNX_GIC_DEBUG_KERNEL_JOBS:-}" \
      bash -lc '
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
  patch
if [ -z "$LNX_GIC_DEBUG_KERNEL_SOURCE" ]; then
  DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends linux-source-7.0.0
fi
rm -rf /tmp/lnx-gic-debug-kernel
mkdir -p /tmp/lnx-gic-debug-kernel
if [ -n "$LNX_GIC_DEBUG_KERNEL_SOURCE" ]; then
  cp -a "$LNX_GIC_DEBUG_KERNEL_SOURCE"/. /tmp/lnx-gic-debug-kernel/
else
  tar xf /usr/src/linux-source-7.0.0.tar.bz2 -C /tmp/lnx-gic-debug-kernel --strip-components=1
fi
cd /tmp/lnx-gic-debug-kernel
if [ -z "${LNX_GIC_DEBUG_SKIP_KERNEL_PATCHES:-}" ]; then
  for patch_file in "$LNX_REPO_ROOT"/kernel-patches/*.patch; do
    patch -p1 < "$patch_file"
  done
fi
for patch_file in "$LNX_REPO_ROOT"/scripts/test/kernel-*-debug.patch; do
  patch -p1 < "$patch_file"
done
cp "$LNX_REPO_ROOT/kernel.config" .config
make olddefconfig
jobs="$LNX_GIC_DEBUG_KERNEL_JOBS"
if [ -z "$jobs" ]; then
  jobs="$(nproc)"
fi
make -j"$jobs"
mkdir -p "$(dirname "$LNX_KERNEL_OUT")"
cp arch/arm64/boot/Image "$LNX_KERNEL_OUT"
'
    printf '%s\n' "$out"
    exit 0
    ;;
  *)
    "$engine" build --platform linux/arm64 -f "$dockerfile" -t "$image" "$repo_root"
    ;;
esac

"$engine" rm -f "$container" >/dev/null 2>&1 || true
"$engine" create --name "$container" "$image" true >/dev/null
"$engine" cp "$container:/build/arch/arm64/boot/Image" "$out"
"$engine" rm "$container" >/dev/null
printf '%s\n' "$out"
