#!/bin/sh
set -eu

target=${1:-}
if [ -z "$target" ]; then
  echo "usage: $0 <kernel|rootfs>" >&2
  exit 2
fi

lnx_bin=${LNX_BIN:-./lnx}
instance=${LNX_BUILD_INSTANCE:-build}

ensure_build_instance() {
  if [ "$instance" = "default" ]; then
    return
  fi
  if "$lnx_bin" --instance "$instance" true >/dev/null 2>&1; then
    return
  fi
  "$lnx_bin" true >/dev/null 2>&1
  "$lnx_bin" clone "$instance" >/dev/null
}

run_in_lnx() {
  ensure_build_instance
  "$lnx_bin" --instance "$instance" sh -lc "$1"
}

case "$target" in
  kernel)
    run_in_lnx '
      set -eu
      podman() {
        sudo -n env PODMAN_IGNORE_CGROUPSV1_WARNING=1 podman "$@"
      }
      podman build --network=host --platform linux/arm64 -f Dockerfile.kernel -t lnx-kernel .
      cid=$(podman create lnx-kernel true)
      cleanup() {
        podman rm -f "$cid" >/dev/null 2>&1 || true
      }
      trap cleanup EXIT INT TERM
      rm -f ./vmlinuz
      podman cp "$cid:/build/arch/arm64/boot/Image" ./vmlinuz
    '
    ;;
  rootfs)
    run_in_lnx '
      set -eu
      podman() {
        sudo -n env PODMAN_IGNORE_CGROUPSV1_WARNING=1 podman "$@"
      }
      podman build --network=host --cap-add=SYS_ADMIN --platform linux/arm64 -f Dockerfile.rootfs -t lnx-rootfs .
      cid=$(podman create lnx-rootfs true)
      cleanup() {
        podman rm -f "$cid" >/dev/null 2>&1 || true
      }
      trap cleanup EXIT INT TERM
      rm -f ./rootfs.ext4
      podman cp "$cid:/rootfs.ext4" ./rootfs.ext4
    '
    ;;
  *)
    echo "unknown build target: $target" >&2
    exit 2
    ;;
esac
