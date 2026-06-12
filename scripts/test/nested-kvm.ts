import { existsSync } from "node:fs";
import { chmod, mkdir, rm } from "node:fs/promises";
import { join } from "node:path";
import {
  assertContains,
  assertEq,
  cleanupInstance,
  cleanupContext,
  defaultContext,
  prepareContext,
  quoteShell,
  run,
  sleep,
  skippableTestStep,
  testStep,
  waitForOwnerExit,
} from "./lib";

const ctx = defaultContext("nested-kvm");
const cwd = join(ctx.repoRoot, `.lnx-nk-${process.pid}`);
const linuxTarget = "aarch64-unknown-linux-musl";
const linuxLnx = join(ctx.repoRoot, "target", linuxTarget, "debug", "lnx");
const linuxGvproxy = join(ctx.repoRoot, "target", "gvproxy-linux-arm64");
const gvproxyUrl = "https://github.com/containers/gvisor-tap-vsock/releases/download/v0.8.9/gvproxy-linux-arm64";
const bunVersion = "1.3.14";
const linuxBunDir = join(ctx.repoRoot, "target", "bun-linux-aarch64");
const linuxBun = join(linuxBunDir, "bun");
const linuxBunUrl = `https://github.com/oven-sh/bun/releases/download/bun-v${bunVersion}/bun-linux-aarch64.zip`;
const linuxLinker = Bun.env.CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER
  ?? Bun.env.CC_LINUX
  ?? "/opt/homebrew/bin/aarch64-linux-musl-gcc";
const hostHome = Bun.env.HOME ?? "";
const kernel = join(hostHome, ".lnx", "vmlinuz");
const rootfs = Bun.env.LNX_NESTED_ROOTFS ?? join(hostHome, ".lnx", "images", "default", "rootfs.ext4");
const snapshotInnerRootfs = join(cwd, "snapshot-inner-rootfs.ext4");
const nestedCheckpointInstance = "lnx-checkpoint-nested";
const nestedSnapshotInstance = "lnx-nested-snapshot";
const nestedScriptInstances = new Map([
  ["scripts/test/nested-system.ts", "lnx-nested-system"],
  ["scripts/test/cp.ts", "lnx-nested-cp"],
  ["scripts/test/nested-checkpoint.ts", nestedCheckpointInstance],
  ["scripts/test/nested-snapshot.ts", nestedSnapshotInstance],
  ["scripts/test/broker-recovery.ts", "lnx-nested-broker-recovery"],
  ["scripts/test/nested-stress.ts", "lnx-nested-stress"],
]);
const outerVmArgs = [
  "--nested-kvm",
  "--kernel",
  kernel,
  "--rootfs",
  rootfs,
  "--cpus",
  "2",
  "--memory-mib",
  "4096",
];
const fullSuite = [
  "scripts/test/system.test.ts",
  "scripts/test/instance-config.test.ts",
  "scripts/test/oci-import.test.ts",
  "scripts/test/cp.test.ts",
  "scripts/test/checkpoint-fork.test.ts",
  "scripts/test/fork-fanout.test.ts",
  "scripts/test/snapshot-compat.test.ts",
  "scripts/test/virtiofs-policy.test.ts",
  "scripts/test/page-cache.test.ts",
  "scripts/test/virtiofs-resume.test.ts",
  "scripts/test/nested-kvm.test.ts",
  "scripts/test/dirty-fs.test.ts",
  "scripts/test/broker-recovery.test.ts",
  "scripts/test/client-chaos.test.ts",
  "scripts/test/pty-resume.test.ts",
  "scripts/test/stress.test.ts",
  "scripts/test/snapshot-chaos.test.ts",
  "scripts/test/stock-ubuntu.test.ts",
  "scripts/test/ingress.test.ts",
  "scripts/test/browser-snapshot.test.ts",
  "scripts/test/privileged-ingress.test.ts",
];

type NestedDisposition =
  | { kind: "run"; testFile: string; script: string }
  | { kind: "partial"; testFile: string; script: string; caveat: string }
  | { kind: "caveat"; testFile: string; caveat: string }
  | { kind: "excluded"; testFile: string; caveat: string };

const nestedDispositions: NestedDisposition[] = [
  {
    kind: "partial",
    testFile: "scripts/test/system.test.ts",
    script: "scripts/test/nested-system.ts",
    caveat: "paths/exec/guest-shape/network coverage runs via scripts/test/nested-system.ts; snapshot restore coverage runs via scripts/test/nested-snapshot.ts",
  },
  {
    kind: "caveat",
    testFile: "scripts/test/instance-config.test.ts",
    caveat: "host-side CLI and descriptor behavior with no guest-specific surface; the macOS-host suite covers it",
  },
  {
    kind: "caveat",
    testFile: "scripts/test/oci-import.test.ts",
    caveat: "pulls from a registry and boots a builder VM; the macOS-host suite covers it and nested guests should not depend on registry availability",
  },
  { kind: "run", testFile: "scripts/test/cp.test.ts", script: "scripts/test/cp.ts" },
  {
    kind: "partial",
    testFile: "scripts/test/checkpoint-fork.test.ts",
    script: "scripts/test/nested-checkpoint.ts",
    caveat: "named checkpoint capture and explicit restore run via scripts/test/nested-checkpoint.ts; full fork cloning is excluded from the nested Linux suite because it duplicates multi-GiB rootfs snapshots over nested virtiofs",
  },
  {
    kind: "caveat",
    testFile: "scripts/test/fork-fanout.test.ts",
    caveat: "checkpoint fanout is still excluded from the nested Linux suite until the new Linux full-RAM snapshot path has more runtime soak",
  },
  {
    kind: "partial",
    testFile: "scripts/test/snapshot-compat.test.ts",
    script: "scripts/test/nested-snapshot.ts",
    caveat: "baseline Linux-host snapshot restore coverage runs via scripts/test/nested-snapshot.ts; malformed-header compatibility logging remains covered by the macOS-host suite until nested Linux restore has more runtime soak",
  },
  {
    kind: "caveat",
    testFile: "scripts/test/virtiofs-policy.test.ts",
    caveat: "contains checkpoint/fork restore checks; Linux virtiofs write allowlist is not enforced today",
  },
  {
    kind: "caveat",
    testFile: "scripts/test/page-cache.test.ts",
    caveat: "asserts idle snapshot completion and rootfs DAX page-cache behavior; nested Linux inner runs use block rootfs",
  },
  {
    kind: "caveat",
    testFile: "scripts/test/virtiofs-resume.test.ts",
    caveat: "open fd/mmap survival is specifically snapshot/fork restore behavior and still needs nested Linux runtime coverage",
  },
  {
    kind: "excluded",
    testFile: "scripts/test/nested-kvm.test.ts",
    caveat: "excluded intentionally to avoid double-nested KVM",
  },
  {
    kind: "caveat",
    testFile: "scripts/test/dirty-fs.test.ts",
    caveat: "depends on checkpoint/fork rootfs snapshots and still needs nested Linux runtime coverage",
  },
  { kind: "run", testFile: "scripts/test/broker-recovery.test.ts", script: "scripts/test/broker-recovery.ts" },
  {
    kind: "caveat",
    testFile: "scripts/test/client-chaos.test.ts",
    caveat: "non-pty disconnect recovery can hang on the follow-up broker command under nested Linux",
  },
  {
    kind: "caveat",
    testFile: "scripts/test/pty-resume.test.ts",
    caveat: "asserts pty survives snapshot-exit; Linux snapshot restore is newly wired and still needs pty-specific nested coverage",
  },
  {
    kind: "partial",
    testFile: "scripts/test/stress.test.ts",
    script: "scripts/test/nested-stress.ts",
    caveat: "parallel channel coverage runs via scripts/test/nested-stress.ts; the snapshot-waits-for-active-channels step remains excluded until Linux snapshot restore has more runtime soak",
  },
  {
    kind: "caveat",
    testFile: "scripts/test/snapshot-chaos.test.ts",
    caveat: "drives randomized workloads across snapshot-exit cycles on the macOS/HVF path; enable for the nested Linux host once snapshot-exit with live channels is reliable there",
  },
  {
    kind: "caveat",
    testFile: "scripts/test/stock-ubuntu.test.ts",
    caveat: "snapd panics while parsing the nested guest kernel command line under nested KVM; a nested stock boot/apt probe also hung instead of producing bounded signal",
  },
  {
    kind: "caveat",
    testFile: "scripts/test/ingress.test.ts",
    caveat: "host-side ingress lifecycle uses macOS launchd/resolver assumptions",
  },
  {
    kind: "caveat",
    testFile: "scripts/test/browser-snapshot.test.ts",
    caveat: "creates checkpoints/forks and is opt-in because it installs browser/compositor packages",
  },
  {
    kind: "caveat",
    testFile: "scripts/test/privileged-ingress.test.ts",
    caveat: "privileged host ingress uses sudo, /etc/resolver, launchd, and privileged ports",
  },
];
const selectedNestedScripts = (Bun.env.LNX_NESTED_ONLY ?? "")
  .split(",")
  .map((name) => name.trim())
  .filter(Boolean);
const nestedSuite = nestedDispositions.flatMap((entry) =>
  entry.kind === "run" || entry.kind === "partial" ? [entry.script] : []
).filter((script) =>
  selectedNestedScripts.length === 0
    || selectedNestedScripts.some((selected) => script === selected || script.endsWith(`/${selected}`))
);
const nestedCaveats = nestedDispositions.flatMap((entry) =>
  entry.kind === "partial" || entry.kind === "caveat" || entry.kind === "excluded"
    ? [[entry.testFile, entry.caveat]]
    : []
);
const dispositionCounts = new Map<string, number>();
for (const entry of nestedDispositions) {
  dispositionCounts.set(entry.testFile, (dispositionCounts.get(entry.testFile) ?? 0) + 1);
}
const missingNestedDisposition = fullSuite.filter((testFile) => !dispositionCounts.has(testFile));
const duplicateNestedDisposition = [...dispositionCounts]
  .filter(([, count]) => count !== 1)
  .map(([testFile]) => testFile);
const unknownNestedDisposition = [...dispositionCounts]
  .map(([testFile]) => testFile)
  .filter((testFile) => !fullSuite.includes(testFile));
if (
  missingNestedDisposition.length > 0
  || duplicateNestedDisposition.length > 0
  || unknownNestedDisposition.length > 0
) {
  throw new Error(
    [
      `missing: ${missingNestedDisposition.join(", ") || "none"}`,
      `duplicate: ${duplicateNestedDisposition.join(", ") || "none"}`,
      `unknown: ${unknownNestedDisposition.join(", ") || "none"}`,
    ].join("; "),
  );
}
const outerInstances: string[] = [];

function outerInstance(name: string): string {
  const instance = `${ctx.instance}-${name}`;
  outerInstances.push(instance);
  return instance;
}

function outerLnx(instance: string, args: string[], options: Parameters<typeof run>[1] = {}) {
  return run([ctx.lnxBin, "--instance", instance, ...args], {
    timeoutMs: 120_000,
    ...options,
    env: {
      LNX_BROKER_IDLE_TTL_MS: "250",
      ...options.env,
    },
  });
}

function stageNestedToolsScript(extraTools: string[] = []): string[] {
  return [
    "nested_tools=/tmp/lnx-nested-tools",
    "rm -rf \"$nested_tools\"",
    "mkdir -p \"$nested_tools\"",
    `cp ${quoteShell(linuxLnx)} "$nested_tools/lnx"`,
    `cp ${quoteShell(linuxGvproxy)} "$nested_tools/gvproxy-linux-arm64"`,
    ...extraTools,
    "chmod +x \"$nested_tools\"/*",
    "export LNX_BIN=\"$nested_tools/lnx\"",
    "export GVPROXY_PATH=\"$nested_tools/gvproxy-linux-arm64\"",
  ];
}

function waitForInnerOwnerScript(): string[] {
  return [
    "wait_for_inner_owner_exit() {",
    "  pidfile=\"$LNX_BASE/instances/$1/bootstrap.lock.d/owner.pid\"",
    "  python3 - \"$pidfile\" <<'PY'",
    "import pathlib, sys, time",
    "pidfile = pathlib.Path(sys.argv[1])",
    "# Sleep between polls: the pidfile lives on virtiofs, and a busy spin",
    "# starves the same virtiofs queue the inner snapshot capture writes",
    "# through, stalling the capture this loop is waiting on.",
    "for _ in range(12_000):",
    "    try:",
    "        pid = int(''.join(ch for ch in pidfile.read_text() if ch.isdigit()))",
    "    except (FileNotFoundError, ValueError):",
    "        raise SystemExit(0)",
    "    proc = pathlib.Path('/proc') / str(pid) / 'cmdline'",
    "    try:",
    "        cmdline = proc.read_bytes().replace(b'\\0', b' ')",
    "    except FileNotFoundError:",
    "        raise SystemExit(0)",
    "    if b'_vm-owner' not in cmdline:",
    "        raise SystemExit(0)",
    "    time.sleep(0.05)",
    "print(f'timeout waiting for inner owner exit: {pidfile}', file=sys.stderr)",
    "raise SystemExit(1)",
    "PY",
    "}",
  ];
}

async function waitForOuterExit(instance: string) {
  await waitForOwnerExit({
    ...ctx,
    instance,
    imageDir: join(ctx.base, "images", instance),
    runDir: join(ctx.base, "instances", instance),
    snapshotDir: join(ctx.base, "images", instance, "memory-snapshots"),
  }, 120_000);
}

async function cloneRootfs(src: string, dest: string) {
  await rm(dest, { force: true });
  await run(["cp", "-c", src, dest], {
    timeoutMs: 180_000,
  });
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

async function cloneShrunkRootfs(src: string, dest: string) {
  await cloneRootfs(src, dest);
  await shrinkRootfsToMinimum(dest);
}

async function prepareInnerBase(base: string, instance: string) {
  await rm(base, { recursive: true, force: true });
  await mkdir(join(base, "images", instance), { recursive: true });
  await run(["cp", kernel, join(base, "vmlinuz")], { timeoutMs: 180_000 });
  await cloneRootfs(snapshotInnerRootfs, join(base, "images", instance, "rootfs.ext4"));
}

async function innerOwnerCounts(base: string, instance: string): Promise<{ starts: number; dones: number }> {
  const log = join(base, "instances", instance, "lnx.log");
  if (!existsSync(log)) {
    return { starts: 0, dones: 0 };
  }
  const text = await Bun.file(log).text();
  return {
    starts: text.match(/owner\.start/g)?.length ?? 0,
    dones: text.match(/owner\.done/g)?.length ?? 0,
  };
}

async function waitForInnerOwnerDone(
  base: string,
  instance: string,
  beforeDones: number,
  timeoutMs = 180_000,
) {
  const deadline = Date.now() + timeoutMs;
  let counts = await innerOwnerCounts(base, instance);
  while (Date.now() < deadline) {
    counts = await innerOwnerCounts(base, instance);
    if (counts.dones > beforeDones && counts.dones >= counts.starts) {
      return;
    }
    await sleep(100);
  }
  throw new Error(
    `timeout waiting for inner owner exit (${instance}; starts=${counts.starts} dones=${counts.dones} before=${beforeDones})`,
  );
}

async function runInnerViaOuter(
  outer: string,
  innerBase: string,
  innerInstance: string,
  innerArgs: string[],
  options: {
    outerNoSnapshotRestore?: boolean;
    prelude?: string[];
    timeoutMs?: number;
  } = {},
) {
  const before = (await innerOwnerCounts(innerBase, innerInstance)).dones;
  const script = [
    "set -euo pipefail",
    "test -c /dev/kvm",
    "test -r /dev/kvm",
    ...(options.prelude ?? []),
    `export LNX_BASE=${quoteShell(innerBase)}`,
    "export LNX_ROOTFS_BACKEND=block",
    "export LNX_BROKER_IDLE_TTL_MS=250",
    ...stageNestedToolsScript(),
    ...waitForInnerOwnerScript(),
    ["\"$LNX_BIN\"", "--instance", quoteShell(innerInstance), "--memory-mib", "512", "--cpus", "1", ...innerArgs.map(quoteShell)].join(" "),
    `wait_for_inner_owner_exit ${quoteShell(innerInstance)}`,
  ].join("\n");
  const result = await outerLnx(
    outer,
    [
      ...outerVmArgs,
      ...(options.outerNoSnapshotRestore ? ["--no-snapshot-restore"] : []),
      "bash",
      "-lc",
      script,
    ],
    { cwd, timeoutMs: options.timeoutMs ?? 300_000 },
  );
  await waitForInnerOwnerDone(innerBase, innerInstance, before);
  await waitForOuterExit(outer);
  return result;
}

try {
  await prepareContext(ctx);
  await rm(cwd, { recursive: true, force: true });
  await mkdir(cwd, { recursive: true });

  await skippableTestStep("compile Linux lnx for nested guest", async () => {
    if (!existsSync(linuxLinker)) {
      throw new Error(`missing Linux target linker: ${linuxLinker}`);
    }
    await run(
      [
        "cargo",
        "build",
        "--target",
        linuxTarget,
      ],
      {
        cwd: ctx.repoRoot,
        timeoutMs: 180_000,
        env: {
          CC_LINUX: linuxLinker,
          CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER: linuxLinker,
        },
      },
    );
  });

  await skippableTestStep("prepare Linux gvproxy for nested guest", async () => {
    if (!existsSync(linuxGvproxy)) {
      await run(["curl", "-fL", "-o", linuxGvproxy, gvproxyUrl], {
        cwd: ctx.repoRoot,
        timeoutMs: 180_000,
      });
    }
    await chmod(linuxGvproxy, 0o755);
  });

  await skippableTestStep("prepare Linux bun for nested suite", async () => {
    if (!existsSync(linuxBun)) {
      const archive = join(ctx.repoRoot, "target", `bun-linux-aarch64-${bunVersion}.zip`);
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
  });

  await testStep("stage writable nested rootfs images", async () => {
    if (!existsSync(rootfs)) {
      throw new Error(`missing rootfs image: ${rootfs}`);
    }
    await cloneShrunkRootfs(rootfs, snapshotInnerRootfs);
  });

  await skippableTestStep("nested KVM test prerequisites exist", async () => {
    if (!existsSync(linuxLnx)) {
      throw new Error(`missing Linux lnx binary: ${linuxLnx}`);
    }
    if (!existsSync(kernel)) {
      throw new Error(`missing kernel image: ${kernel}`);
    }
    if (!existsSync(rootfs)) {
      throw new Error(`missing rootfs image: ${rootfs}`);
    }
    if (!existsSync(linuxGvproxy)) {
      throw new Error(`missing Linux gvproxy binary: ${linuxGvproxy}`);
    }
    if (!existsSync(linuxBun)) {
      throw new Error(`missing Linux bun binary: ${linuxBun}`);
    }
  });

  await testStep("boot lnx inside lnx after outer snapshot resume", async () => {
    const innerBase = join(cwd, "s");
    const innerInstance = `si-${process.pid}`;
    await prepareInnerBase(innerBase, innerInstance);
    const instance = outerInstance("resume");
    const result = await runInnerViaOuter(
      instance,
      innerBase,
      innerInstance,
      ["--no-snapshot-restore", "uname", "-m"],
      {
        outerNoSnapshotRestore: true,
        prelude: ["lnxctl snapshot-exit"],
        timeoutMs: 300_000,
      },
    );
    assertContains(result.stdout, "aarch64", "inner lnx booted through nested KVM after resume");
  });

  if (Bun.env.LNX_NESTED_SKIP_RESTORE === "1") {
    process.stderr.write(
      "test restore lnx snapshot inside nested-capable guest ... SKIP (LNX_NESTED_SKIP_RESTORE=1)\n",
    );
  } else await testStep("restore lnx snapshot inside nested-capable guest", async () => {
    const innerBase = join(cwd, "restore");
    const innerInstance = `ri-${process.pid}`;
    const outer = outerInstance("restore");
    await prepareInnerBase(innerBase, innerInstance);

    const cold = await runInnerViaOuter(
      outer,
      innerBase,
      innerInstance,
      [
        "--no-snapshot-restore",
        "bash",
        "-lc",
        "printf nested-disk >/home/lnxuser/nested-kvm-state && cat /home/lnxuser/nested-kvm-state",
      ],
      { outerNoSnapshotRestore: true },
    );
    assertEq(cold.stdout, "nested-disk", "inner cold write");

    const restored = await runInnerViaOuter(
      outer,
      innerBase,
      innerInstance,
      ["bash", "-lc", "cat /home/lnxuser/nested-kvm-state && printf /restored"],
    );
    assertEq(restored.stdout, "nested-disk/restored", "inner restored disk state");
  });

  if (Bun.env.LNX_NESTED_RUN_FULL_SUITE !== "1") {
    process.stderr.write(
      "test run Linux-host-compatible suite in nested-capable guest ... SKIP (set LNX_NESTED_RUN_FULL_SUITE=1)\n",
    );
  } else await testStep("run Linux-host-compatible suite in nested-capable guest", async () => {
    const suiteBase = join(cwd, "suite");
    const suiteDefaultImage = join(suiteBase, "images", "default");
    const suiteKernel = join(suiteBase, "vmlinuz");
    const suiteRootfs = join(suiteDefaultImage, "rootfs.ext4");
    const suiteLog = join(cwd, "nested-suite.log");
    await rm(suiteBase, { recursive: true, force: true });
    await rm(suiteLog, { force: true });
    await mkdir(suiteDefaultImage, { recursive: true });
    await run(["cp", kernel, suiteKernel], { timeoutMs: 180_000 });
    await cloneShrunkRootfs(rootfs, suiteRootfs);
    for (const instance of new Set(nestedSuite.map((script) => nestedScriptInstances.get(script)))) {
      if (!instance) {
        continue;
      }
      const image = join(suiteBase, "images", instance);
      await mkdir(image, { recursive: true });
      await cloneRootfs(suiteRootfs, join(image, "rootfs.ext4"));
    }

    const script = [
      "set -euo pipefail",
      "test -c /dev/kvm",
      "test -r /dev/kvm",
      ...stageNestedToolsScript([`cp ${quoteShell(linuxBun)} "$nested_tools/bun"`]),
      "export PATH=\"$nested_tools:$PATH\"",
      "command -v bun >/dev/null",
      `cd ${quoteShell(ctx.repoRoot)}`,
      "rm -rf /tmp/lnx-nested-kvm-cargo-target",
      "export CARGO_TARGET_DIR=/tmp/lnx-nested-kvm-cargo-target",
      `export LNX_BASE=${quoteShell(suiteBase)}`,
      "export LNX_ROOTFS_BACKEND=block",
      "export LNX_BROKER_IDLE_TTL_MS=250",
      "export LNX_SKIP_TEST_CLEANUP=1",
      `suite_log=${quoteShell(suiteLog)}`,
      ": > \"$suite_log\"",
      "run_logged() {",
      "  echo \"+ $*\" >> \"$suite_log\"",
      "  echo \"nested-run: $*\" >&2",
      "  \"$@\" >> \"$suite_log\" 2>&1 || {",
      "    status=$?",
      "    tail -200 \"$suite_log\" >&2 || true",
      "    return \"$status\"",
      "  }",
      "}",
      ...waitForInnerOwnerScript(),
      "cat >> \"$suite_log\" <<'NESTED_CAVEATS'",
      "nested-linux caveats:",
      ...nestedCaveats.map(([testFile, reason]) => `- ${testFile}: ${reason}`),
      "NESTED_CAVEATS",
      "for test_file in \\",
      ...nestedSuite.map((testFile, index) =>
        `  ${quoteShell(testFile)}${index === nestedSuite.length - 1 ? "" : " \\"}`
      ),
      "do",
      "  case \"$test_file\" in",
      `    scripts/test/nested-system.ts) export LNX_TEST_INSTANCE=${quoteShell(nestedScriptInstances.get("scripts/test/nested-system.ts")!)}; unset LNX_TEST_CPUS; export LNX_TEST_MEMORY_MIB=1024; export LNX_BROKER_IDLE_TTL_MS=250 ;;`,
      `    scripts/test/cp.ts) export LNX_TEST_INSTANCE=${quoteShell(nestedScriptInstances.get("scripts/test/cp.ts")!)}; unset LNX_TEST_CPUS; export LNX_TEST_MEMORY_MIB=1024; export LNX_BROKER_IDLE_TTL_MS=250 ;;`,
      `    scripts/test/nested-checkpoint.ts) export LNX_TEST_INSTANCE=${quoteShell(nestedCheckpointInstance)}; unset LNX_TEST_CPUS; export LNX_TEST_MEMORY_MIB=1024; export LNX_BROKER_IDLE_TTL_MS=5000 ;;`,
      `    scripts/test/nested-snapshot.ts) export LNX_TEST_INSTANCE=${quoteShell(nestedSnapshotInstance)}; unset LNX_TEST_CPUS; export LNX_TEST_MEMORY_MIB=1024; export LNX_BROKER_IDLE_TTL_MS=250 ;;`,
      `    scripts/test/broker-recovery.ts) export LNX_TEST_INSTANCE=${quoteShell(nestedScriptInstances.get("scripts/test/broker-recovery.ts")!)}; unset LNX_TEST_CPUS; export LNX_TEST_MEMORY_MIB=1024; export LNX_BROKER_IDLE_TTL_MS=250 ;;`,
      `    scripts/test/nested-stress.ts) export LNX_TEST_INSTANCE=${quoteShell(nestedScriptInstances.get("scripts/test/nested-stress.ts")!)}; unset LNX_TEST_CPUS; export LNX_TEST_MEMORY_MIB=1024; export LNX_BROKER_IDLE_TTL_MS=250 ;;`,
      "    *) unset LNX_TEST_INSTANCE LNX_TEST_CPUS LNX_TEST_MEMORY_MIB; export LNX_BROKER_IDLE_TTL_MS=250 ;;",
      "  esac",
      "  run_logged timeout --kill-after=5s 360s bun \"$test_file\"",
      "  if [ -n \"${LNX_TEST_INSTANCE:-}\" ]; then",
      "    wait_for_inner_owner_exit \"$LNX_TEST_INSTANCE\"",
      "  fi",
      "done",
      "echo NESTED_SUITE_OK",
    ].join("\n");

    const result = await outerLnx(
      outerInstance("suite"),
      [
        ...outerVmArgs,
        "--no-snapshot-restore",
        "bash",
        "-lc",
        script,
      ],
      { cwd: ctx.repoRoot, timeoutMs: 1_500_000 },
    );
    assertContains(result.stdout, "NESTED_SUITE_OK", "nested guest Linux-host-compatible suite completed");
  });
} finally {
  await Promise.all(outerInstances.map((instance) => cleanupInstance(ctx, instance)));
  await cleanupContext(ctx);
  if (Bun.env.LNX_PRESERVE_NESTED_KVM !== "1") {
    await rm(cwd, { recursive: true, force: true });
  }
}
