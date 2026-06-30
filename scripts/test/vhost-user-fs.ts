import {
  cp,
  mkdir,
  readFile,
  writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { join } from "node:path";
import {
  assertContains,
  assertEq,
  cleanupContext,
  commandExists,
  defaultContext,
  prepareContext,
  run,
  skip,
  sleep,
  spawn,
  testStep,
  waitForVmSuspend,
} from "./lib";

const ctx = defaultContext("vhost-user-fs");
const fixtureDir = join(ctx.repoRoot, "scripts/test/fixtures/vhost-user-fs");
const fixtureWorkDir = join(ctx.tmpdir, "fixture-src");
const fixtureBin = join(ctx.tmpdir, "vhost-user-fs");
const backendRoot = join(ctx.tmpdir, "backend-root");
const socketPath = join(ctx.tmpdir, "testfs.sock");
const patchedGoFuseDir = join(ctx.tmpdir, "go-fuse-v2");
const goFuseFixtureDir = join(ctx.repoRoot, "scripts/test/fixtures/vhost-user-fs-go-fuse");
const goFuseReplacementsDir = join(goFuseFixtureDir, "replacements");
const goFuseOverlayDir = join(goFuseFixtureDir, "overlay");

type GoFuseReplacement = {
  target: string;
  before: string;
  after: string;
  label: string;
};

const goFuseReplacements: GoFuseReplacement[] = [
  {
    target: "internal/vhostuser/deviceregion.go",
    before: "deviceregion-import.before",
    after: "deviceregion-import.after",
    label: "Darwin device-region imports",
  },
  {
    target: "internal/vhostuser/deviceregion.go",
    before: "deviceregion-madvise.before",
    after: "deviceregion-madvise.after",
    label: "Darwin device-region madvise",
  },
  {
    target: "virtiofs/virtiofs.go",
    before: "virtiofs-debug.before",
    after: "virtiofs-debug.after",
    label: "virtiofs debug logging",
  },
  {
    target: "internal/vhostuser/util.go",
    before: "util-read-loop.before",
    after: "util-read-loop.after",
    label: "vhost-user request dispatch",
  },
  {
    target: "internal/vhostuser/virtq.go",
    before: "virtq-eventfd.before",
    after: "virtq-eventfd.after",
    label: "vhost-user eventfd write handling",
  },
  {
    target: "internal/vhostuser/device.go",
    before: "device-vring-base.before",
    after: "device-vring-base.after",
    label: "vhost-user vring base support",
  },
  {
    target: "internal/vhostuser/server.go",
    before: "server-vring-base-dispatch-open.before",
    after: "server-vring-base-dispatch-open.after",
    label: "vhost-user vring base dispatch",
  },
  {
    target: "internal/vhostuser/server.go",
    before: "server-vring-base-dispatch-close.before",
    after: "server-vring-base-dispatch-close.after",
    label: "vhost-user vring base dispatch close",
  },
  {
    target: "fuse/print_darwin.go",
    before: "print-darwin-flags.before",
    after: "print-darwin-flags.after",
    label: "Darwin fuse flag names",
  },
];

async function waitForSocket(path: string): Promise<void> {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    if (existsSync(path)) return;
    await sleep(50);
  }
  throw new Error(`timeout waiting for vhost-user socket ${path}`);
}

function joinRelative(root: string, relativePath: string): string {
  return join(root, ...relativePath.split("/"));
}

async function applyGoFuseReplacement(replacement: GoFuseReplacement): Promise<void> {
  const target = joinRelative(patchedGoFuseDir, replacement.target);
  const before = await readFile(join(goFuseReplacementsDir, replacement.before), "utf8");
  const after = await readFile(join(goFuseReplacementsDir, replacement.after), "utf8");
  const source = await readFile(target, "utf8");
  if (!source.includes(before)) {
    throw new Error(`failed to patch go-fuse ${replacement.label}`);
  }
  await writeFile(target, source.replace(before, after));
}

async function preparePatchedGoFuse(): Promise<void> {
  const moduleInfo = await run(["go", "mod", "download", "-json", "github.com/hanwen/go-fuse/v2"], {
    cwd: fixtureDir,
    timeoutMs: 120_000,
  });
  const sourceDir = JSON.parse(moduleInfo.stdout).Dir;
  await cp(sourceDir, patchedGoFuseDir, { recursive: true });
  await run(["chmod", "-R", "u+w", patchedGoFuseDir], { timeoutMs: 120_000 });

  for (const replacement of goFuseReplacements) {
    await applyGoFuseReplacement(replacement);
  }
  await cp(goFuseOverlayDir, patchedGoFuseDir, { recursive: true, force: true });
}

try {
  if (!(await commandExists("go"))) {
    await skip("go is required for the vhost-user fs fixture");
  }

  await prepareContext(ctx);
  await cp(fixtureDir, fixtureWorkDir, { recursive: true });
  await mkdir(backendRoot, { recursive: true });
  await writeFile(join(backendRoot, "hello.txt"), "hello from host\n");

  await testStep("build go-fuse vhost-user fs fixture", async () => {
    await preparePatchedGoFuse();
    await run(["go", "mod", "edit", `-replace=github.com/hanwen/go-fuse/v2=${patchedGoFuseDir}`], {
      cwd: fixtureWorkDir,
      timeoutMs: 120_000,
    });
    await run(["go", "build", "-o", fixtureBin, "."], {
      cwd: fixtureWorkDir,
      timeoutMs: 120_000,
    });
  });

  const server = spawn([fixtureBin, "-socket", socketPath, "-root", backendRoot], {
    stderr: "inherit",
  });
  try {
    await waitForSocket(socketPath);

    await testStep("guest reconnects external vhost-user virtio-fs after live snapshot resume", async () => {
      const result = await ctx.vm.cli([
        "--no-host-shares",
        "--vhost-user-fs",
        `tag=testfs,mount=/mnt/testfs,socket=${socketPath}`,
        "sh",
        "-lc",
        [
          "set -euo pipefail",
          "cat /mnt/testfs/hello.txt",
          "lnxctl snapshot-exit",
          "cat /mnt/testfs/hello.txt",
          "if sh -c \"printf 'hello from guest' > /mnt/testfs/guest.txt\" 2>/tmp/write.err; then echo write-succeeded; exit 1; fi",
          "cat /tmp/write.err",
        ].join("; "),
      ]);
      assertEq(
        result.stdout.split("\n").filter((line) => line === "hello from host").length,
        2,
        "guest read host file before and after live snapshot resume",
      );
      assertContains(result.stdout, "Read-only file system", "guest write was rejected");
      assertEq(existsSync(join(backendRoot, "guest.txt")), false, "host did not receive guest write");
      await waitForVmSuspend(ctx);
    });

    await testStep("guest restores external vhost-user virtio-fs snapshot", async () => {
      const result = await ctx.vm.cli([
        "--no-host-shares",
        "--vhost-user-fs",
        `tag=testfs,mount=/mnt/testfs,socket=${socketPath}`,
        "cat",
        "/mnt/testfs/hello.txt",
      ]);
      assertContains(result.stdout, "hello from host", "restored guest read host file");
    });
  } finally {
    server.kill();
    await server.exited.catch(() => {});
  }
} finally {
  await cleanupContext(ctx);
}
