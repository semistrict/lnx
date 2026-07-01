import {
  existsSync } from "node:fs";
import { chmod,
  mkdir,
  rm,
  writeFile } from "node:fs/promises";
import { join } from "node:path";
import {
  assertContains,
  cleanupContext,
  cleanupInstance,
  cloneSparseImage,
  defaultContext,
  diskUsageBytes,
  fileSize,
  prepareContext,
  quoteShell,
  run,
  testStep,
  waitForOwnerExit,
} from "./lib";

const ctx = defaultContext("nested-deterministic-time");
const cwd = join(ctx.repoRoot, `.lnx-ndt-${process.pid}`);
const linuxTarget = "aarch64-unknown-linux-musl";
const linuxLnx = join(ctx.repoRoot, "target", linuxTarget, "debug", "lnx");
const linuxGvproxy = join(ctx.repoRoot, "target", "gvproxy-linux-arm64");
const gvproxyUrl =
  "https://github.com/containers/gvisor-tap-vsock/releases/download/v0.8.9/gvproxy-linux-arm64";
const bunVersion = "1.3.14";
const linuxBunDir = join(ctx.repoRoot, "target", "bun-linux-aarch64");
const linuxBun = join(linuxBunDir, "bun");
const linuxBunUrl = `https://github.com/oven-sh/bun/releases/download/bun-v${bunVersion}/bun-linux-aarch64.zip`;
const linuxLinker =
  Bun.env.CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER ??
  Bun.env.CC_LINUX ??
  "/opt/homebrew/bin/aarch64-linux-musl-gcc";
const hostHome = Bun.env.HOME ?? "";
const managedKernel = join(hostHome, ".lnx", "vmlinuz");
const outerKernel = Bun.env.LNX_NESTED_OUTER_KERNEL ?? defaultOuterKernel();
const innerKernel = Bun.env.LNX_NESTED_INNER_KERNEL ?? managedKernel;
const rootfs =
  Bun.env.LNX_NESTED_ROOTFS ??
  join(hostHome, ".lnx", "cache", "rootfs.ext4");
const outerInstance = `${ctx.instance}-outer`;
const suiteBase = join(cwd, "inner-base");
const outerRootfs = join(cwd, "outer-rootfs.ext4");
const outerRootfsBytes = Number(
  Bun.env.LNX_NESTED_OUTER_ROOTFS_BYTES ?? 16 * 1024 * 1024 * 1024,
);
const suiteRootfsBytes = Number(
  Bun.env.LNX_NESTED_SUITE_ROOTFS_BYTES ?? 2 * 1024 * 1024 * 1024,
);
const suiteCache = join(suiteBase, "cache", "rootfs.ext4");
const suiteDefault = join(suiteBase, "instances", "default", "rootfs.ext4");
const outerLocalSuiteBase = `/root/lnx-nested-deterministic-inner-${process.pid}`;
const outerVmArgs = [
  "--nested-kvm",
  "--kernel",
  outerKernel,
  "--rootfs",
  outerRootfs,
  "--cpus",
  "2",
  "--memory-mib",
  "4096",
];

function defaultOuterKernel(): string {
  const result = Bun.spawnSync(["scripts/kernel-build.sh", "image-path"], {
    cwd: ctx.repoRoot,
    stdout: "pipe",
    stderr: "pipe",
  });
  if (result.exitCode === 0) {
    return new TextDecoder().decode(result.stdout).trim();
  }
  return join(
    ctx.repoRoot,
    "target",
    `vmlinuz-${Bun.env.LNX_KERNEL_VERSION ?? "7.1.2"}`,
  );
}

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
  const fsck = await run([e2fsTool("e2fsck"), "-fy", path], {
    check: false,
    timeoutMs: 180_000,
  });
  if ((fsck.status & ~3) !== 0) {
    throw new Error(
      `e2fsck failed (${fsck.status}): ${fsck.stderr || fsck.stdout}`,
    );
  }
  await run([e2fsTool("resize2fs"), "-M", path], { timeoutMs: 180_000 });
  await alignRootfsForPmem(path);
  const postResizeFsck = await run([e2fsTool("e2fsck"), "-fy", path], {
    check: false,
    timeoutMs: 180_000,
  });
  if ((postResizeFsck.status & ~3) !== 0) {
    throw new Error(
      `post-resize e2fsck failed (${postResizeFsck.status}): ${postResizeFsck.stderr || postResizeFsck.stdout}`,
    );
  }
}

async function alignRootfsForPmem(path: string) {
  await run([
    "python3",
    "-c",
    "import os, sys\nalign = 2 * 1024 * 1024\npath = sys.argv[1]\nsize = os.path.getsize(path)\nos.truncate(path, ((size + align - 1) // align) * align)\n",
    path,
  ]);
}

async function fsckRootfs(path: string, label: string) {
  const fsck = await run([e2fsTool("e2fsck"), "-fy", path], {
    check: false,
    timeoutMs: 180_000,
  });
  if ((fsck.status & ~3) !== 0) {
    throw new Error(
      `${label} e2fsck failed (${fsck.status}): ${fsck.stderr || fsck.stdout}`,
    );
  }
}

async function cloneShrunkRootfs(src: string, dest: string) {
  await cloneSparseImage(src, dest);
  await shrinkRootfsToMinimum(dest);
}

async function growRootfs(path: string, sizeBytes: number) {
  await run(["truncate", "-s", String(sizeBytes), path], {
    timeoutMs: 180_000,
  });
  await run([e2fsTool("resize2fs"), path], { timeoutMs: 180_000 });
  await alignRootfsForPmem(path);
  await fsckRootfs(path, "post-grow");
}

async function assertSparseVmImage(path: string, label: string) {
  const size = await fileSize(path);
  const allocated = await diskUsageBytes(path);
  if (size >= 8 * 1024 * 1024 * 1024 && allocated > size / 2) {
    throw new Error(
      `${label} is not sparse enough: size=${size} allocated=${allocated}`,
    );
  }
}

async function ensureLinuxTools() {
  if (!existsSync(linuxLinker)) {
    throw new Error(`missing Linux target linker: ${linuxLinker}`);
  }
  await run(["scripts/prepare-nested-helpers.sh", "debug"], {
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

  if (!existsSync(linuxBun)) {
    const archive = join(
      ctx.repoRoot,
      "target",
      `bun-linux-aarch64-${bunVersion}.zip`,
    );
    await run(["curl", "-fL", "-o", archive, linuxBunUrl], {
      cwd: ctx.repoRoot,
      timeoutMs: 180_000,
    });
    await run(["unzip", "-oq", archive, "-d", join(ctx.repoRoot, "target")], {
      cwd: ctx.repoRoot,
      timeoutMs: 180_000,
    });
  }
  await chmod(linuxBun, 0o755);
}

async function prepareInnerBase() {
  if (!existsSync(outerKernel)) {
    throw new Error(
      `missing outer kernel image: ${outerKernel}; run bun run kernel:ensure-lnx`,
    );
  }
  if (!existsSync(innerKernel)) {
    throw new Error(`missing inner kernel image: ${innerKernel}`);
  }
  if (!existsSync(rootfs)) {
    throw new Error(`missing rootfs image: ${rootfs}`);
  }
  await rm(suiteBase, { recursive: true, force: true });
  await mkdir(join(suiteBase, "cache"), { recursive: true });
  await mkdir(join(suiteBase, "instances", "default"), { recursive: true });
  await run(["cp", innerKernel, join(suiteBase, "vmlinuz")], { timeoutMs: 180_000 });
  await cloneShrunkRootfs(rootfs, outerRootfs);
  await growRootfs(outerRootfs, outerRootfsBytes);
  await cloneShrunkRootfs(rootfs, suiteCache);
  await growRootfs(suiteCache, suiteRootfsBytes);
  await cloneSparseImage(suiteCache, suiteDefault);
  await writeFile(join(suiteBase, "instances", "default", "vm-initialized"), "1\n");
  await assertSparseVmImage(suiteCache, "nested deterministic cache rootfs");
  await assertSparseVmImage(suiteDefault, "nested deterministic default rootfs");
}

function stageNestedToolsScript(): string[] {
  return [
    "nested_tools=/tmp/lnx-nested-tools",
    'rm -rf "$nested_tools"',
    'mkdir -p "$nested_tools"',
    `cp ${quoteShell(linuxLnx)} "$nested_tools/lnx"`,
    `cp ${quoteShell(linuxGvproxy)} "$nested_tools/gvproxy-linux-arm64"`,
    `cp ${quoteShell(linuxBun)} "$nested_tools/bun"`,
    'chmod +x "$nested_tools"/*',
    'export PATH="$nested_tools:$PATH"',
    'export LNX_BIN="$nested_tools/lnx"',
    'export GVPROXY_PATH="$nested_tools/gvproxy-linux-arm64"',
  ];
}

function stageInnerBaseScript(): string[] {
  const localDefault = `${outerLocalSuiteBase}/instances/default/rootfs.ext4`;
  return [
    `rm -rf ${quoteShell(outerLocalSuiteBase)}`,
    `mkdir -p ${quoteShell(`${outerLocalSuiteBase}/instances/default`)}`,
    `cp ${quoteShell(join(suiteBase, "vmlinuz"))} ${quoteShell(`${outerLocalSuiteBase}/vmlinuz`)}`,
    `"$LNX_BIN" _sparse-copy ${quoteShell(suiteDefault)} ${quoteShell(localDefault)}`,
    `cp ${quoteShell(join(suiteBase, "instances/default/vm-initialized"))} ${quoteShell(`${outerLocalSuiteBase}/instances/default/vm-initialized`)}`,
  ];
}

async function waitForOuterExit() {
  await waitForOwnerExit(
    {
      ...ctx,
      instance: outerInstance,
      imageDir: join(ctx.base, "instances", outerInstance),
      runDir: join(ctx.base, "instances", outerInstance),
      snapshotDir: join(
        ctx.base,
        "instances",
        outerInstance,
        "memory-snapshots",
      ),
    },
    120_000,
  );
}

try {
  await prepareContext(ctx);
  await cleanupInstance(ctx, outerInstance);
  await rm(cwd, { recursive: true, force: true });
  await mkdir(cwd, { recursive: true });

  await testStep("prepare nested deterministic prerequisites", async () => {
    await ensureLinuxTools();
    await prepareInnerBase();
  });

  await testStep("deterministic time works inside nested lnx", async () => {
    const script = [
      "set -euo pipefail",
      "test -c /dev/kvm",
      "test -r /dev/kvm",
      ...stageNestedToolsScript(),
      ...stageInnerBaseScript(),
      "command -v bun >/dev/null",
      `export LNX_BASE=${quoteShell(outerLocalSuiteBase)}`,
      "export LNX_TEST_INSTANCE=default",
      "export LNX_TEST_CPUS=1",
      "export LNX_TEST_MEMORY_MIB=512",
      "export LNX_BROKER_IDLE_TTL_MS=250",
      "export LNX_SKIP_TEST_CLEANUP=1",
      `cd ${quoteShell(ctx.repoRoot)}`,
      "bun scripts/test/nested-deterministic-inner.ts",
      "echo NESTED_DETERMINISTIC_TIME_OK",
    ].join("\n");
    const result = await run(
      [ctx.lnxBin, "--instance", outerInstance, "--root", ...outerVmArgs, "bash", "-lc", script],
      {
        cwd: ctx.repoRoot,
        timeoutMs: 900_000,
        env: {
          LNX_BROKER_IDLE_TTL_MS: "250",
          LNX_INGRESS_STATE_DIR: join(cwd, "disabled-ingress"),
        },
      },
    );
    assertContains(
      result.stdout,
      "NESTED_DETERMINISTIC_TIME_OK",
      "nested deterministic time completion",
    );
    await waitForOuterExit();
  });
} finally {
  await cleanupContext(ctx);
  await cleanupInstance(ctx, outerInstance);
  await rm(cwd, { recursive: true, force: true });
}
