import { existsSync } from "node:fs";
import { chmod, mkdir, readdir, readFile, rm } from "node:fs/promises";
import { dirname, join } from "node:path";
import {
  assertContains,
  assertEq,
  cloneSparseImage,
  cleanupContext,
  defaultContext,
  prepareContext,
  quoteShell,
  run,
  waitForOwnerExit,
} from "./lib";

if (process.platform !== "darwin") {
  throw new Error("Linux snapshot fixture creation currently runs from a macOS host with nested KVM");
}

const ctx = defaultContext("linux-snapshot-fixture");
const output =
  Bun.env.LNX_LINUX_SNAPSHOT_FIXTURE_OUT ??
  join(ctx.repoRoot, "target", "linux-macos-snapshot-fixture");
const cwd = join(ctx.repoRoot, "target", `lnx-linux-fixture-${process.pid}`);
const linuxTarget = "aarch64-unknown-linux-musl";
const linuxLnx = join(ctx.repoRoot, "target", linuxTarget, "debug", "lnx");
const linuxLinker =
  Bun.env.CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER ??
  Bun.env.CC_LINUX ??
  "/opt/homebrew/bin/aarch64-linux-musl-gcc";
const linuxGvproxy = join(ctx.repoRoot, "target", "gvproxy-linux-arm64");
const gvproxyUrl =
  "https://github.com/containers/gvisor-tap-vsock/releases/download/v0.8.9/gvproxy-linux-arm64";
const hostHome = Bun.env.HOME ?? "";
const kernel = Bun.env.LNX_NESTED_INNER_KERNEL ?? join(hostHome, ".lnx", "vmlinuz");
const outerKernel = Bun.env.LNX_NESTED_OUTER_KERNEL ?? join(hostHome, ".lnx", "vmlinuz");
const rootfs =
  Bun.env.LNX_NESTED_ROOTFS ?? join(hostHome, ".lnx", "instances", "default", "rootfs.ext4");
const innerBase = join(cwd, "inner-base");
const innerRunBase = `/tmp/lnx-run-linux-fixture-${process.pid}`;
const innerInstance = `linux-fixture-${process.pid}`;
const checkpointName = Bun.env.LNX_LINUX_SNAPSHOT_CHECKPOINT_NAME ?? "linux-macos-fixture";

function e2fsTool(name: string): string {
  for (const dir of [
    "/opt/homebrew/opt/e2fsprogs/sbin",
    "/usr/local/opt/e2fsprogs/sbin",
    "/opt/homebrew/sbin",
    "/usr/local/sbin",
  ]) {
    const candidate = join(dir, name);
    if (existsSync(candidate)) {
      return candidate;
    }
  }
  return name;
}

async function shrinkRootfsToMinimum(path: string) {
  const e2fsck = e2fsTool("e2fsck");
  const resize2fs = e2fsTool("resize2fs");
  const fsck = await run([e2fsck, "-fy", path], {
    check: false,
    timeoutMs: 180_000,
  });
  if ((fsck.status & ~3) !== 0) {
    throw new Error(`e2fsck failed (${fsck.status}): ${fsck.stderr || fsck.stdout}`);
  }
  await run([resize2fs, "-M", path], {
    timeoutMs: 180_000,
  });
}

async function checkpointPathByName(imageDir: string, name: string): Promise<string> {
  const checkpointDir = join(imageDir, "checkpoints");
  for (const entry of await readdir(checkpointDir, { withFileTypes: true })) {
    if (!entry.isDirectory()) {
      continue;
    }
    const path = join(checkpointDir, entry.name);
    const meta = await readFile(join(path, "checkpoint.meta"), "utf8");
    if (meta.split("\n").includes(`name=${name}`)) {
      return path;
    }
  }
  throw new Error(`checkpoint not found: ${name}`);
}

async function ensureLinuxTools() {
  if (!existsSync(linuxLinker)) {
    throw new Error(`missing Linux target linker: ${linuxLinker}`);
  }
  await run(["cargo", "build", "--target", linuxTarget], {
    cwd: ctx.repoRoot,
    timeoutMs: 180_000,
    env: {
      CC_LINUX: linuxLinker,
      CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER: linuxLinker,
    },
  });

  if (!existsSync(linuxGvproxy)) {
    await run(["curl", "-fL", "-o", linuxGvproxy, gvproxyUrl], {
      cwd: ctx.repoRoot,
      timeoutMs: 180_000,
    });
  }
  await chmod(linuxGvproxy, 0o755);
}

function outerScript(): string {
  const sourcePython = String.raw`
import subprocess
import time
from pathlib import Path

if "arm64.nopauth" not in Path("/proc/cmdline").read_text():
    raise SystemExit("portable snapshot source missing arm64.nopauth")

subprocess.run(["sudo", "tee", "/root/lnx-cross-host-disk"], input=b"linux-disk", stdout=subprocess.DEVNULL, check=True)
subprocess.run(["sudo", "tee", "/run/lnx-cross-host-memory"], input=b"linux-memory", stdout=subprocess.DEVNULL, check=True)
print("linux-source-ready", flush=True)

go = Path("/run/lnx-cross-host-go")
deadline = time.time() + 300
while time.time() < deadline and not go.exists():
    time.sleep(0.1)
if not go.exists():
    raise SystemExit("resume signal timed out")

subprocess.run(["sudo", "tee", "/root/lnx-cross-host-after"], input=b"linux-after", stdout=subprocess.DEVNULL, check=True)
print("linux-source-after", flush=True)
`;

  return [
    "set -euo pipefail",
    "test -c /dev/kvm",
    `export LNX_BASE=${quoteShell(innerBase)}`,
    `export LNX_RUN_BASE=${quoteShell(innerRunBase)}`,
    "rm -rf \"$LNX_RUN_BASE\"",
    "nested_tools=/tmp/lnx-linux-fixture-tools",
    "rm -rf \"$nested_tools\"",
    "mkdir -p \"$nested_tools\"",
    `cp ${quoteShell(linuxLnx)} "$nested_tools/lnx"`,
    `cp ${quoteShell(linuxGvproxy)} "$nested_tools/gvproxy-linux-arm64"`,
    "chmod +x \"$nested_tools\"/*",
    "export LNX_BIN=\"$nested_tools/lnx\"",
    "export GVPROXY_PATH=\"$nested_tools/gvproxy-linux-arm64\"",
    "export LNX_ROOTFS_BACKEND=block",
    "export LNX_BROKER_IDLE_TTL_MS=250",
    `inner_instance=${quoteShell(innerInstance)}`,
    `checkpointName=${quoteShell(checkpointName)}`,
    "source_out=/tmp/lnx-linux-fixture-source.out",
    "source_err=/tmp/lnx-linux-fixture-source.err",
    "rm -f \"$source_out\" \"$source_err\"",
    "\"$LNX_BIN\" --instance \"$inner_instance\" --no-host-shares --memory-mib 512 --cpus 1 python3 - >\"$source_out\" 2>\"$source_err\" <<'PY' &",
    sourcePython,
    "PY",
    "source_pid=$!",
    "for i in $(seq 1 1800); do",
    "  if grep -q linux-source-ready \"$source_out\" 2>/dev/null; then break; fi",
    "  if ! kill -0 \"$source_pid\" 2>/dev/null; then cat \"$source_out\"; cat \"$source_err\" >&2; wait \"$source_pid\"; exit 1; fi",
    "  sleep 0.1",
    "done",
    "grep -q linux-source-ready \"$source_out\" || { cat \"$source_out\"; cat \"$source_err\" >&2; exit 1; }",
    "\"$LNX_BIN\" --instance \"$inner_instance\" --no-host-shares checkpoint -m \"$checkpointName\"",
    "\"$LNX_BIN\" --instance \"$inner_instance\" --no-host-shares sudo sh -c 'printf go >/run/lnx-cross-host-go'",
    "wait \"$source_pid\"",
    "grep -q linux-source-after \"$source_out\" || { cat \"$source_out\"; cat \"$source_err\" >&2; exit 1; }",
    "pidfile=\"$LNX_RUN_BASE/instances/$inner_instance/bootstrap.lock.d/owner.pid\"",
    "for i in $(seq 1 1200); do",
    "  [ ! -e \"$pidfile\" ] && break",
    "  sleep 0.1",
    "done",
  ].join("\n");
}

try {
  await prepareContext(ctx);
  await rm(cwd, { recursive: true, force: true });
  await rm(output, { recursive: true, force: true });
  await mkdir(dirname(output), { recursive: true });
  await mkdir(join(innerBase, "instances", innerInstance), { recursive: true });

  for (const path of [kernel, outerKernel, rootfs]) {
    if (!existsSync(path)) {
      throw new Error(`missing fixture prerequisite: ${path}`);
    }
  }

  await ensureLinuxTools();
  await run(["cp", kernel, join(innerBase, "vmlinuz")], { timeoutMs: 180_000 });
  const innerRootfs = join(innerBase, "instances", innerInstance, "rootfs.ext4");
  await cloneSparseImage(rootfs, innerRootfs);
  await shrinkRootfsToMinimum(innerRootfs);

  await run(
    [
      ctx.lnxBin,
      "--instance",
      ctx.instance,
      "--nested-kvm",
      "--kernel",
      outerKernel,
      "--rootfs",
      rootfs,
      "--cpus",
      "2",
      "--memory-mib",
      "4096",
      "_vm-init",
    ],
    {
      cwd,
      timeoutMs: 180_000,
      env: {
        LNX_BROKER_IDLE_TTL_MS: "250",
      },
    },
  );
  await waitForOwnerExit(ctx, 120_000);
  await rm(join(ctx.imageDir, "memory-snapshots", "latest"), {
    recursive: true,
    force: true,
  });

  await run(
    [
      ctx.lnxBin,
      "--instance",
      ctx.instance,
      "--nested-kvm",
      "--kernel",
      outerKernel,
      "--rootfs",
      rootfs,
      "--cpus",
      "2",
      "--memory-mib",
      "4096",
      "--root",
      "bash",
      "-lc",
      outerScript(),
    ],
    {
      cwd,
      timeoutMs: 420_000,
      env: {
        LNX_BROKER_IDLE_TTL_MS: "250",
      },
    },
  );

  const snapshot = await checkpointPathByName(join(innerBase, "instances", innerInstance), checkpointName);
  for (const file of ["vmstate.bin", "pages.img", "rootfs.ext4", "shares.stamp", "initramfs.stamp"]) {
    const path = join(snapshot, file);
    if (!existsSync(path)) {
      throw new Error(`missing Linux snapshot file: ${path}`);
    }
  }
  const vmstate = await Bun.file(join(snapshot, "vmstate.bin")).arrayBuffer();
  assertEq(new DataView(vmstate).getUint32(8, true), 4, "source snapshot is shared vmstate v4");
  const sharesStamp = await Bun.file(join(snapshot, "shares.stamp")).text();
  assertContains(sharesStamp, "host-shares=disabled-v1", "source snapshot has host shares disabled");
  assertContains(sharesStamp, "net=gvproxy", "source snapshot uses portable gvproxy backing");

  await mkdir(output, { recursive: true });
  await cloneSparseImage(join(snapshot, "rootfs.ext4"), join(output, "rootfs.ext4"));
  await cloneSparseImage(join(snapshot, "pages.img"), join(output, "pages.img"));
  for (const file of ["vmstate.bin", "shares.stamp", "initramfs.stamp"]) {
    await run(["cp", join(snapshot, file), join(output, file)], { timeoutMs: 180_000 });
  }
  await run(["cp", kernel, join(output, "vmlinuz")], { timeoutMs: 180_000 });

  console.log(`LNX_LINUX_SNAPSHOT_FIXTURE=${output}`);
} finally {
  await cleanupContext(ctx);
  if (Bun.env.LNX_SKIP_TEST_CLEANUP !== "1") {
    await rm(cwd, { recursive: true, force: true });
  }
}
