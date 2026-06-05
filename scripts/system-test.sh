#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
lnx_bin="${LNX_BIN:-$repo_root/target/debug/lnx}"
instance="${LNX_TEST_INSTANCE:-system-test-$$}"
copy_instance="$instance-copy"
base="${LNX_BASE:-$HOME/.lnx}"
image_dir="$base/images/$instance"
run_dir="$base/instances/$instance"
snapshot_dir="$image_dir/memory-snapshots"
copy_image_dir="$base/images/$copy_instance"
copy_run_dir="$base/instances/$copy_instance"

if [[ ! -x "$lnx_bin" ]]; then
  echo "missing lnx binary: $lnx_bin" >&2
  exit 1
fi

tmpdir="$(mktemp -d "${TMPDIR:-/tmp}/lnx-system-test.XXXXXX")"

cleanup() {
  rm -rf "$tmpdir" "$image_dir" "$run_dir" "$copy_image_dir" "$copy_run_dir"
}
trap cleanup EXIT
cleanup
mkdir -p "$tmpdir"

run_lnx() {
  "$lnx_bin" --instance "$instance" "$@"
}

assert_eq() {
  local got="$1"
  local want="$2"
  local label="$3"
  if [[ "$got" != "$want" ]]; then
    printf 'FAIL %s: got <%s>, want <%s>\n' "$label" "$got" "$want" >&2
    exit 1
  fi
}

assert_contains() {
  local haystack="$1"
  local needle="$2"
  local label="$3"
  if [[ "$haystack" != *"$needle"* ]]; then
    printf 'FAIL %s: <%s> does not contain <%s>\n' "$label" "$haystack" "$needle" >&2
    exit 1
  fi
}

assert_file() {
  local path="$1"
  local label="$2"
  if [[ ! -f "$path" ]]; then
    printf 'FAIL %s: missing file %s\n' "$label" "$path" >&2
    exit 1
  fi
}

paths_output="$("$lnx_bin" --instance "$instance" paths)"
assert_contains "$paths_output" "name: $instance" "paths prints instance name"
assert_contains "$paths_output" "rootfs: $image_dir/rootfs.ext4" "paths prints rootfs"
assert_contains "$paths_output" "snapshots: $snapshot_dir" "paths prints snapshots"

env_paths_output="$(LNX_INSTANCE="$instance" "$lnx_bin" paths)"
assert_contains "$env_paths_output" "name: $instance" "paths honors LNX_INSTANCE"

override_paths_output="$("$lnx_bin" --instance "$instance" --kernel "$tmpdir/kernel.override" --rootfs "$tmpdir/rootfs.override" paths)"
assert_contains "$override_paths_output" "kernel: $tmpdir/kernel.override" "paths honors explicit kernel"
assert_contains "$override_paths_output" "rootfs: $tmpdir/rootfs.override" "paths honors explicit rootfs"

printf 'kernel-copy-test' >"$tmpdir/kernel"
printf 'rootfs-copy-test' >"$tmpdir/rootfs.ext4"
"$lnx_bin" --instance "$copy_instance" --kernel "$tmpdir/copied-kernel" --rootfs "$tmpdir/copied-rootfs.ext4" init --kernel "$tmpdir/kernel" --rootfs "$tmpdir/rootfs.ext4"
assert_eq "$(cat "$tmpdir/copied-kernel")" "kernel-copy-test" "explicit init copied kernel"
assert_eq "$(cat "$tmpdir/copied-rootfs.ext4")" "rootfs-copy-test" "explicit init copied rootfs"

cold="$(run_lnx --no-snapshot-restore echo cold)"
assert_eq "$cold" "cold" "cold exec"
assert_file "$base/vmlinuz" "auto-init kernel"
assert_file "$image_dir/rootfs.ext4" "auto-init rootfs"
assert_file "$snapshot_dir/latest/vmstate.bin" "full snapshot vmstate"
assert_file "$snapshot_dir/latest/pages.img" "full snapshot pages"
assert_file "$snapshot_dir/latest/rootfs.ext4" "full snapshot rootfs"

restored="$(run_lnx echo restored)"
assert_eq "$restored" "restored" "restored exec"

run_subcommand="$(run_lnx run echo run-subcommand)"
assert_eq "$run_subcommand" "run-subcommand" "run subcommand exec"

explicit_snapshot="$(run_lnx --snapshot "$snapshot_dir/latest" echo explicit-snapshot)"
assert_eq "$explicit_snapshot" "explicit-snapshot" "explicit snapshot restore"

marker="$(run_lnx bash -lc 'printf memory > /run/lnx-memory-marker; printf disk > /root/lnx-disk-marker; echo marker-written')"
assert_eq "$marker" "marker-written" "marker write"
marker_read="$(run_lnx bash -lc 'printf "%s/%s" "$(cat /run/lnx-memory-marker)" "$(cat /root/lnx-disk-marker)"')"
assert_eq "$marker_read" "memory/disk" "memory and disk snapshot persistence"

piped="$(printf 'stdin-ok' | run_lnx cat)"
assert_eq "$piped" "stdin-ok" "non-pty stdin"

shell_output="$(printf 'echo noargs-shell; exit\n' | run_lnx)"
assert_eq "$shell_output" "noargs-shell" "default shell over stdin"

set +e
run_lnx bash -lc 'echo stdout-line; echo stderr-line >&2; exit 7' >"$tmpdir/exit.out" 2>"$tmpdir/exit.err"
exit_status=$?
set -e
assert_eq "$exit_status" "7" "exit status propagation"
assert_eq "$(cat "$tmpdir/exit.out")" "stdout-line" "stdout propagation"
assert_eq "$(cat "$tmpdir/exit.err")" "stderr-line" "stderr propagation"

set +e
run_lnx definitely-not-a-command >"$tmpdir/notfound.out" 2>"$tmpdir/notfound.err"
notfound_status=$?
set -e
assert_eq "$notfound_status" "127" "command-not-found status"
assert_contains "$(cat "$tmpdir/notfound.err")" "exec failed" "command-not-found stderr"

pty_output="$(python3 - "$lnx_bin" "$instance" <<'PY'
import errno
import fcntl
import os
import pty
import select
import struct
import subprocess
import sys
import termios

lnx_bin, instance = sys.argv[1], sys.argv[2]
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 33, 101, 0, 0))
env = os.environ.copy()
env["TERM"] = "xterm-256color"
env["COLORTERM"] = "truecolor"
cmd = [
    lnx_bin,
    "--instance",
    instance,
    "bash",
    "-lc",
    'printf "PTY_OK\\n"; if test -t 0 && test -t 1; then echo TTY=yes; else echo TTY=no; fi; printf "SIZE="; stty size; printf "TERM=$TERM COLORTERM=${COLORTERM:-}\\n"',
]
proc = subprocess.Popen(cmd, stdin=slave, stdout=slave, stderr=slave, close_fds=True, env=env)
os.close(slave)
output = bytearray()
while True:
    ready, _, _ = select.select([master], [], [], 15)
    if not ready:
        proc.kill()
        raise SystemExit("timeout waiting for PTY command")
    try:
        chunk = os.read(master, 4096)
    except OSError as exc:
        if exc.errno == errno.EIO:
            break
        raise
    if not chunk:
        break
    output.extend(chunk)
    if proc.poll() is not None:
        continue
status = proc.wait(timeout=5)
os.close(master)
sys.stdout.write(output.decode(errors="replace").replace("\r\n", "\n"))
raise SystemExit(status)
PY
)"
assert_contains "$pty_output" "PTY_OK" "pty command output"
assert_contains "$pty_output" "TTY=yes" "pty tty detection"
assert_contains "$pty_output" "SIZE=33 101" "pty window size"
assert_contains "$pty_output" "TERM=xterm-256color" "pty TERM"
assert_contains "$pty_output" "COLORTERM=truecolor" "pty COLORTERM"

lnxctl_usage_status=0
set +e
run_lnx lnxctl --help >"$tmpdir/lnxctl-help.out" 2>"$tmpdir/lnxctl-help.err"
lnxctl_usage_status=$?
set -e
assert_eq "$lnxctl_usage_status" "2" "lnxctl usage status"
assert_contains "$(cat "$tmpdir/lnxctl-help.err")" "usage: lnxctl snapshot-exit" "lnxctl usage text"

lnxctl_status=0
set +e
run_lnx lnxctl snapshot-exit >"$tmpdir/lnxctl.out" 2>"$tmpdir/lnxctl.err"
lnxctl_status=$?
set -e
assert_eq "$lnxctl_status" "0" "lnxctl snapshot-exit status"
post_lnxctl="$(run_lnx echo post-lnxctl)"
assert_eq "$post_lnxctl" "post-lnxctl" "exec after lnxctl snapshot-exit"

page_size="$(run_lnx getconf PAGESIZE)"
assert_eq "$page_size" "16384" "guest page size"

cpu_count="$(run_lnx nproc)"
assert_eq "$cpu_count" "2" "default cpu count"

memory_mib="$(run_lnx bash -lc 'free -m | awk "/^Mem:/ {print \$2}"')"
if (( memory_mib < 3900 || memory_mib > 4100 )); then
  printf 'FAIL default memory MiB: <%s>\n' "$memory_mib" >&2
  exit 1
fi

root_mount="$(run_lnx findmnt -n -o FSTYPE,OPTIONS /)"
if [[ "$root_mount" != ext4*"dax=always"* ]]; then
  printf 'FAIL pmem root mount: <%s>\n' "$root_mount" >&2
  exit 1
fi

network_probe="$(run_lnx bash -lc 'tmp=/tmp/lnx-network-probe; rm -f "$tmp"; curl -fsS --max-time 20 -o "$tmp" http://ports.ubuntu.com/ubuntu-ports/dists/resolute/InRelease; sed -n "1p" "$tmp"')"
assert_eq "$network_probe" "-----BEGIN PGP SIGNED MESSAGE-----" "outbound networking"

run_lnx bash -lc 'sleep 1; echo slow' >"$tmpdir/slow.out" 2>"$tmpdir/slow.err" &
slow_pid=$!
run_lnx echo fast >"$tmpdir/fast.out" 2>"$tmpdir/fast.err" &
fast_pid=$!
run_lnx bash -lc 'printf channel-ok' >"$tmpdir/channel.out" 2>"$tmpdir/channel.err" &
channel_pid=$!

wait "$slow_pid"
wait "$fast_pid"
wait "$channel_pid"

assert_eq "$(cat "$tmpdir/slow.out")" "slow" "parallel slow channel"
assert_eq "$(cat "$tmpdir/fast.out")" "fast" "parallel fast channel"
assert_eq "$(cat "$tmpdir/channel.out")" "channel-ok" "parallel shell channel"

run_lnx bash -lc 'sleep 1; echo delayed' >"$tmpdir/delayed.out" 2>"$tmpdir/delayed.err" &
delayed_pid=$!
run_lnx lnxctl snapshot-exit >"$tmpdir/lnxctl-parallel.out" 2>"$tmpdir/lnxctl-parallel.err"
wait "$delayed_pid"
assert_eq "$(cat "$tmpdir/delayed.out")" "delayed" "lnxctl waits for active channels"

fresh_state="$(run_lnx --no-snapshot-restore bash -lc 'if [ -e /run/lnx-memory-marker ]; then echo stale; else echo fresh; fi')"
assert_eq "$fresh_state" "fresh" "no-snapshot restore starts fresh memory"

final="$(run_lnx echo final)"
assert_eq "$final" "final" "post-parallel exec"

assert_file "$run_dir/timings.log" "timings log"
assert_file "$run_dir/lnx.log" "run log"
assert_contains "$(tail -n 200 "$run_dir/lnx.log")" "snapshot.done" "snapshot logged"

if [[ -f "$run_dir/console.log" ]] \
  && grep -E 'Out of memory|Killed process|rcu:|timer wakeup|stall' "$run_dir/console.log" >/dev/null; then
  echo "FAIL console log contains kernel stall/OOM markers" >&2
  grep -E 'Out of memory|Killed process|rcu:|timer wakeup|stall' "$run_dir/console.log" >&2
  exit 1
fi

echo "system tests passed for instance $instance"
