import { cp, mkdir } from "node:fs/promises";
import { join } from "node:path";
import { platform } from "node:process";
import {
  assertEq,
  cloneSparseImage,
  cleanupContext,
  cleanupInstance,
  defaultContext,
  lnx,
  prepareContext,
  testStep,
} from "./lib";

const ctx = defaultContext("deterministic-time");
const secondInstance = `${ctx.instance}-again`;
const secondCtx = {
  ...ctx,
  instance: secondInstance,
  imageDir: join(ctx.base, "instances", secondInstance),
  runDir: join(Bun.env.LNX_RUN_BASE ?? ctx.base, "instances", secondInstance),
  snapshotDir: join(ctx.base, "instances", secondInstance, "memory-snapshots"),
};
const vmArgs = [
  ...(Bun.env.LNX_TEST_CPUS ? ["--cpus", Bun.env.LNX_TEST_CPUS] : []),
  ...(Bun.env.LNX_TEST_MEMORY_MIB
    ? ["--memory-mib", Bun.env.LNX_TEST_MEMORY_MIB]
    : []),
];

if (platform !== "linux") {
  console.log("skipping deterministic-time: --deterministic is KVM-only");
  process.exit(0);
}

function deterministicEnv(instance: string) {
  return {
    LNX_BROKER_IDLE_TTL_MS: "10000",
    LNX_INGRESS_STATE_DIR: join(ctx.tmpdir, `ingress-${instance}`),
  };
}

async function prepareDeterministicCheckpoint(instance: string) {
  const probeCtx = { ...ctx, instance };
  await lnx(
    probeCtx,
    [...vmArgs, "--deterministic=seed42", "--trace-events", "true"],
    {
      timeoutMs: 180_000,
      env: deterministicEnv(instance),
    },
  );
}

async function deterministicProbe(instance: string) {
  const probeCtx = { ...ctx, instance };
  const started = performance.now();
  const result = await lnx(
    probeCtx,
    [
      ...vmArgs,
      "--deterministic=seed42",
      "--trace-events",
      "bash",
      "-lc",
      [
        "set -euo pipefail",
        "a=$(date +%s%N)",
        "sleep 20",
        "b=$(date +%s%N)",
        'printf "delta_s=%s\\n" "$(((b - a) / 1000000000))"',
      ].join("; "),
    ],
    {
      timeoutMs: 180_000,
      env: deterministicEnv(instance),
    },
  );
  return { stdout: result.stdout, elapsedMs: performance.now() - started };
}

async function clonePreparedCheckpoint() {
  await mkdir(secondCtx.imageDir, { recursive: true });
  await cloneSparseImage(
    join(ctx.imageDir, "rootfs.ext4"),
    join(secondCtx.imageDir, "rootfs.ext4"),
  );
  for (const name of [
    "lnx.json",
    "shares.stamp",
    "deterministic.stamp",
    "initramfs.cpio",
    "initramfs.stamp",
  ]) {
    await cp(join(ctx.imageDir, name), join(secondCtx.imageDir, name), {
      preserveTimestamps: true,
    });
  }
  const sourceSnapshot = join(ctx.imageDir, "memory-snapshots", "latest");
  const destSnapshot = join(secondCtx.imageDir, "memory-snapshots", "latest");
  await mkdir(destSnapshot, { recursive: true });
  for (const name of [
    "shares.stamp",
    "deterministic.stamp",
    "initramfs.stamp",
    "deterministic-clock.state",
  ]) {
    await cp(join(sourceSnapshot, name), join(destSnapshot, name), {
      preserveTimestamps: true,
    });
  }
  for (const name of ["rootfs.ext4", "pages.img", "vmstate.bin"]) {
    await cloneSparseImage(join(sourceSnapshot, name), join(destSnapshot, name));
  }
}

function parseDeltaSeconds(stdout: string): number {
  const match = stdout.match(/^delta_s=([0-9]+)$/m);
  if (!match) {
    throw new Error(`missing deterministic time delta in output:\n${stdout}`);
  }
  return Number(match[1]);
}

try {
  await prepareContext(ctx);
  await cleanupInstance(ctx, secondInstance);

  let first = { stdout: "", elapsedMs: 0 };
  await testStep("deterministic sleep advances guest time without host wait", async () => {
    await prepareDeterministicCheckpoint(ctx.instance);
    await clonePreparedCheckpoint();
    first = await deterministicProbe(ctx.instance);
    const delta = parseDeltaSeconds(first.stdout);
    if (delta < 20) {
      throw new Error(`guest time did not advance by sleep duration:\n${first.stdout}`);
    }
    if (first.elapsedMs > 15_000) {
      throw new Error(
        `deterministic sleep used host wall time: ${Math.round(first.elapsedMs)}ms\n${first.stdout}`,
      );
    }
  });

  await testStep("same seed produces same guest time output", async () => {
    const second = await deterministicProbe(secondInstance);
    assertEq(second.stdout, first.stdout, "same seed guest time output");
  });
} finally {
  await cleanupContext(ctx);
  await cleanupInstance(ctx, secondInstance);
}
