import { existsSync } from "node:fs";
import { chmod, mkdir, rm } from "node:fs/promises";
import { join } from "node:path";
import {
  assertContains,
  cleanupInstance,
  cleanupContext,
  defaultContext,
  prepareContext,
  quoteShell,
  run,
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
const rootfs = join(hostHome, ".lnx", "images", "default", "rootfs.ext4");
const snapshotInnerRootfs = join(cwd, "snapshot-inner-rootfs.ext4");
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
    caveat: "non-snapshot paths/exec/guest-shape/network coverage runs via scripts/test/nested-system.ts; post-command snapshot and explicit snapshot restore checks remain excluded because Linux libkrun snapshot APIs return ENOSYS",
  },
  { kind: "run", testFile: "scripts/test/cp.test.ts", script: "scripts/test/cp.ts" },
  {
    kind: "caveat",
    testFile: "scripts/test/checkpoint-fork.test.ts",
    caveat: "checkpoint/fork requires snapshot capture/restore; Linux libkrun snapshot APIs return ENOSYS",
  },
  {
    kind: "caveat",
    testFile: "scripts/test/fork-fanout.test.ts",
    caveat: "checkpoint fanout requires snapshot capture/restore; Linux libkrun snapshot APIs return ENOSYS",
  },
  {
    kind: "caveat",
    testFile: "scripts/test/snapshot-compat.test.ts",
    caveat: "directly validates snapshot restore compatibility; Linux libkrun snapshot APIs return ENOSYS",
  },
  {
    kind: "caveat",
    testFile: "scripts/test/virtiofs-policy.test.ts",
    caveat: "contains checkpoint/fork restore checks; Linux libkrun snapshot APIs return ENOSYS; Linux virtiofs write allowlist is not enforced today",
  },
  {
    kind: "caveat",
    testFile: "scripts/test/page-cache.test.ts",
    caveat: "asserts idle snapshot completion and rootfs DAX page-cache behavior; nested Linux inner runs use block rootfs",
  },
  {
    kind: "caveat",
    testFile: "scripts/test/virtiofs-resume.test.ts",
    caveat: "open fd/mmap survival is specifically snapshot/fork restore behavior; Linux libkrun snapshot APIs return ENOSYS",
  },
  {
    kind: "excluded",
    testFile: "scripts/test/nested-kvm.test.ts",
    caveat: "excluded intentionally to avoid double-nested KVM",
  },
  {
    kind: "caveat",
    testFile: "scripts/test/dirty-fs.test.ts",
    caveat: "depends on checkpoint/fork rootfs snapshots; Linux libkrun snapshot APIs return ENOSYS",
  },
  { kind: "run", testFile: "scripts/test/broker-recovery.test.ts", script: "scripts/test/broker-recovery.ts" },
  { kind: "run", testFile: "scripts/test/client-chaos.test.ts", script: "scripts/test/client-chaos.ts" },
  {
    kind: "caveat",
    testFile: "scripts/test/pty-resume.test.ts",
    caveat: "asserts pty survives snapshot-exit; Linux libkrun snapshot APIs return ENOSYS",
  },
  {
    kind: "partial",
    testFile: "scripts/test/stress.test.ts",
    script: "scripts/test/nested-stress.ts",
    caveat: "parallel channel coverage runs via scripts/test/nested-stress.ts; the snapshot-waits-for-active-channels step remains excluded because Linux libkrun snapshot APIs return ENOSYS",
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
const nestedSuite = nestedDispositions.flatMap((entry) =>
  entry.kind === "run" || entry.kind === "partial" ? [entry.script] : []
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

async function waitForOuterExit(instance: string) {
  await waitForOwnerExit({
    ...ctx,
    instance,
    imageDir: join(ctx.base, "images", instance),
    runDir: join(ctx.base, "instances", instance),
    snapshotDir: join(ctx.base, "images", instance, "memory-snapshots"),
  });
}

async function cloneRootfs(src: string, dest: string) {
  await rm(dest, { force: true });
  await run(["cp", "-c", src, dest], {
    timeoutMs: 180_000,
  });
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
    await cloneRootfs(rootfs, snapshotInnerRootfs);
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
    const script = [
      "set -euo pipefail",
      "test -c /dev/kvm",
      "test -r /dev/kvm",
      "lnxctl snapshot-exit",
      `rm -rf ${quoteShell(innerBase)}`,
      `export LNX_BASE=${quoteShell(innerBase)}`,
      `export GVPROXY_PATH=${quoteShell(linuxGvproxy)}`,
      "export LNX_ROOTFS_BACKEND=block",
      "export LNX_BROKER_IDLE_TTL_MS=250",
      [
        quoteShell(linuxLnx),
        "--instance",
        quoteShell(innerInstance),
        "--kernel",
        quoteShell(kernel),
        "--rootfs",
        quoteShell(snapshotInnerRootfs),
        "--no-snapshot-restore",
        "--cpus",
        "1",
        "--memory-mib",
        "1024",
        "uname",
        "-m",
      ].join(" "),
    ].join("\n");

    const instance = outerInstance("resume");
    const result = await outerLnx(
      instance,
      [
        ...outerVmArgs,
        "--no-snapshot-restore",
        "bash",
        "-lc",
        script,
      ],
      { cwd, timeoutMs: 240_000 },
    );
    assertContains(result.stdout, "aarch64", "inner lnx booted through nested KVM after resume");
    await waitForOuterExit(instance);
  });

  await testStep("run Linux-host-compatible suite in nested-capable guest", async () => {
    const suiteBase = join(cwd, "suite");
    const suiteDefaultImage = join(suiteBase, "images", "default");
    const suiteKernel = join(suiteBase, "vmlinuz");
    const suiteRootfs = join(suiteDefaultImage, "rootfs.ext4");
    const suiteLog = join(cwd, "nested-suite.log");
    await rm(suiteBase, { recursive: true, force: true });
    await rm(suiteLog, { force: true });
    await mkdir(suiteDefaultImage, { recursive: true });
    await run(["cp", kernel, suiteKernel], { timeoutMs: 180_000 });
    await cloneRootfs(rootfs, suiteRootfs);

    const script = [
      "set -euo pipefail",
      "test -c /dev/kvm",
      "test -r /dev/kvm",
      `export PATH=${quoteShell(linuxBunDir)}:$PATH`,
      "command -v bun >/dev/null",
      `cd ${quoteShell(ctx.repoRoot)}`,
      "rm -rf /tmp/lnx-nested-kvm-cargo-target",
      "export CARGO_TARGET_DIR=/tmp/lnx-nested-kvm-cargo-target",
      `export LNX_BASE=${quoteShell(suiteBase)}`,
      `export LNX_BIN=${quoteShell(linuxLnx)}`,
      `export GVPROXY_PATH=${quoteShell(linuxGvproxy)}`,
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
      "cat >> \"$suite_log\" <<'NESTED_CAVEATS'",
      "nested-linux caveats:",
      ...nestedCaveats.map(([testFile, reason]) => `- ${testFile}: ${reason}`),
      "NESTED_CAVEATS",
      "for test_file in \\",
      ...nestedSuite.map((testFile, index) =>
        `  ${quoteShell(testFile)}${index === nestedSuite.length - 1 ? "" : " \\"}`
      ),
      "do",
      "  run_logged timeout --kill-after=5s 240s bun \"$test_file\"",
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
  await rm(cwd, { recursive: true, force: true });
}
