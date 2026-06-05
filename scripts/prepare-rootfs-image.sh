#!/usr/bin/env bash
set -euo pipefail

rootfs="${1:?usage: prepare-rootfs-image.sh ROOTFS [SIZE]}"
size="${2:-64G}"

if [[ ! -f "$rootfs" ]]; then
  echo "missing rootfs image: $rootfs" >&2
  exit 1
fi

find_tool() {
  local name="$1"
  local dir
  if command -v "$name" >/dev/null 2>&1; then
    command -v "$name"
    return
  fi
  for dir in \
    /opt/homebrew/opt/e2fsprogs/sbin \
    /usr/local/opt/e2fsprogs/sbin \
    /opt/homebrew/sbin \
    /usr/local/sbin
  do
    if [[ -x "$dir/$name" ]]; then
      printf '%s\n' "$dir/$name"
      return
    fi
  done
  echo "missing required tool: $name" >&2
  exit 1
}

file_size() {
  if stat -c %s "$rootfs" >/dev/null 2>&1; then
    stat -c %s "$rootfs"
  else
    stat -f %z "$rootfs"
  fi
}

e2fsck="$(find_tool e2fsck)"
resize2fs="$(find_tool resize2fs)"
dumpe2fs="$(find_tool dumpe2fs)"

echo "rootfs: grow logical size to $size"
truncate -s "$size" "$rootfs"

echo "rootfs: check filesystem"
set +e
"$e2fsck" -fy "$rootfs"
status=$?
set -e
if (( (status & ~3) != 0 )); then
  echo "e2fsck failed with status $status" >&2
  exit "$status"
fi

echo "rootfs: resize filesystem"
"$resize2fs" "$rootfs"

block_size="$("$dumpe2fs" -h "$rootfs" 2>/dev/null | awk -F: '/Block size:/ {gsub(/^[ \t]+/, "", $2); print $2; exit}')"
if [[ "$block_size" != "16384" ]]; then
  echo "rootfs has ext4 block size $block_size, expected 16384" >&2
  exit 1
fi

bytes="$(file_size)"
if (( bytes < 68719476736 )); then
  echo "rootfs is $bytes bytes, expected at least 68719476736" >&2
  exit 1
fi

if command -v fallocate >/dev/null 2>&1; then
  echo "rootfs: dig sparse holes"
  fallocate -d "$rootfs"
fi

ls -lh "$rootfs"
du -h "$rootfs"
