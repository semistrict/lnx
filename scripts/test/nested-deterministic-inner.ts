import {
  existsSync } from "node:fs";
import { readdir,
  readFile,
  rm } from "node:fs/promises";
import { join } from "node:path";
import {
  assertEq,
  defaultContext,
  run,
  sleep,
  testStep,
} from "./lib";

const ctx = defaultContext("nested-deterministic-inner");
const sourceInstance = Bun.env.LNX_TEST_INSTANCE ?? "default";
const replayInstance = `${sourceInstance}-again`;
const checkpointName = "deterministic-base";
const vmArgs = [
  ...(Bun.env.LNX_TEST_CPUS ? ["--cpus", Bun.env.LNX_TEST_CPUS] : []),
  ...(Bun.env.LNX_TEST_MEMORY_MIB
    ? ["--memory-mib", Bun.env.LNX_TEST_MEMORY_MIB]
    : []),
];

function instanceDir(instance: string) {
  return join(ctx.base, "instances", instance);
}

function instanceRunDir(instance: string) {
  return join(Bun.env.LNX_RUN_BASE ?? ctx.base, "instances", instance);
}

function deterministicEnv() {
  return {
    LNX_BROKER_IDLE_TTL_MS: "600000",
    LNX_INGRESS_STATE_DIR: join(ctx.tmpdir, "ingress"),
  };
}

function lnxCommand(
  instance: string,
  args: string[],
  options: Parameters<typeof run>[1] = {},
) {
  return run([ctx.lnxBin, "--instance", instance, ...vmArgs, ...args], {
    timeoutMs: 180_000,
    ...options,
    env: {
      ...deterministicEnv(),
      ...options.env,
    },
  });
}

async function stopOwner(instance: string) {
  const pidfile = join(instanceRunDir(instance), "bootstrap.lock.d", "owner.pid");
  if (!existsSync(pidfile)) {
    return;
  }
  const raw = await readFile(pidfile, "utf8");
  const pid = Number(raw.replaceAll(/\D/g, ""));
  if (!Number.isInteger(pid) || pid <= 0 || !existsSync(`/proc/${pid}`)) {
    return;
  }
  try {
    process.kill(pid, "SIGTERM");
  } catch {
    return;
  }
  for (let i = 0; i < 200; i += 1) {
    if (!existsSync(`/proc/${pid}`)) {
      return;
    }
    await sleep(50);
  }
  try {
    process.kill(pid, "SIGKILL");
  } catch {
    return;
  }
}

async function checkpointPathByName(name: string): Promise<string> {
  const checkpointDir = join(instanceDir(sourceInstance), "checkpoints");
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

function parseDeltaSeconds(stdout: string): number {
  const match = stdout.match(/^delta_s=([0-9]+)$/m);
  if (!match) {
    throw new Error(`missing deterministic time delta in output:\n${stdout}`);
  }
  return Number(match[1]);
}

async function deterministicProbe(instance: string, checkpoint: string) {
  const started = performance.now();
  const result = await lnxCommand(instance, [
    "--rootfs",
    join(checkpoint, "rootfs.ext4"),
    "--snapshot",
    checkpoint,
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
  ]);
  await stopOwner(instance);
  return { stdout: result.stdout, elapsedMs: performance.now() - started };
}

try {
  await rm(join(instanceDir(sourceInstance), "checkpoints"), {
    recursive: true,
    force: true,
  });
  await rm(instanceDir(replayInstance), { recursive: true, force: true });
  await rm(instanceRunDir(replayInstance), { recursive: true, force: true });
  await rm(ctx.tmpdir, { recursive: true, force: true });

  await testStep("create deterministic checkpoint through nested broker", async () => {
    const warm = await lnxCommand(sourceInstance, [
      "--deterministic=seed42",
      "--trace-events",
      "bash",
      "-lc",
      "printf ready",
    ]);
    assertEq(warm.stdout, "ready", "deterministic warmup output");

    const checkpoint = await lnxCommand(sourceInstance, [
      "--deterministic=seed42",
      "--trace-events",
      "checkpoint",
      "-m",
      checkpointName,
    ]);
    assertEq(checkpoint.stdout, checkpointName, "checkpoint name");
    await stopOwner(sourceInstance);
  });

  await testStep("deterministic checkpoint replay is instant and stable", async () => {
    const checkpoint = await checkpointPathByName(checkpointName);
    const first = await deterministicProbe(sourceInstance, checkpoint);
    const delta = parseDeltaSeconds(first.stdout);
    if (delta < 20) {
      throw new Error(`guest time did not advance by sleep duration:\n${first.stdout}`);
    }
    if (first.elapsedMs > 15_000) {
      throw new Error(
        `deterministic sleep used host wall time: ${Math.round(first.elapsedMs)}ms\n${first.stdout}`,
      );
    }

    const second = await deterministicProbe(replayInstance, checkpoint);
    assertEq(second.stdout, first.stdout, "same seed guest time output");
  });
} finally {
  await stopOwner(sourceInstance);
  await stopOwner(replayInstance);
}
