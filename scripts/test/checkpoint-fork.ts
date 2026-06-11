import { join } from "node:path";
import {
  assertContains,
  assertEq,
  assertFile,
  cleanupContext,
  defaultContext,
  lnx,
  prepareContext,
  run,
  testStep,
} from "./lib";

const ctx = defaultContext("checkpoint");
const forkA = `${ctx.instance}-from-named`;
const forkB = `${ctx.instance}-from-current`;
const forkAImage = join(ctx.base, "images", forkA);
const forkARun = join(ctx.base, "instances", forkA);
const forkBImage = join(ctx.base, "images", forkB);
const forkBRun = join(ctx.base, "instances", forkB);
const vmArgs = [
  ...(Bun.env.LNX_TEST_CPUS ? ["--cpus", Bun.env.LNX_TEST_CPUS] : []),
  ...(Bun.env.LNX_TEST_MEMORY_MIB ? ["--memory-mib", Bun.env.LNX_TEST_MEMORY_MIB] : []),
];

function lnxVm(args: string[], options: Parameters<typeof lnx>[1] = {}) {
  return lnx(ctx, [...vmArgs, ...args], options);
}

function lnxCommand(instance: string, args: string[], options: Parameters<typeof run>[1] = {}) {
  return run([ctx.lnxBin, "--instance", instance, ...vmArgs, ...args], options);
}

async function cleanupForks() {
  await run(["rm", "-rf", forkAImage, forkARun, forkBImage, forkBRun], { check: false });
}

try {
  await prepareContext(ctx);
  await cleanupForks();

  await testStep("checkpoint captures disk and memory", async () => {
    const write = await lnxVm([
      "--no-snapshot-restore",
      "bash",
      "-lc",
      "printf disk-before | sudo tee /root/lnx-checkpoint-disk >/dev/null; printf memory-before | sudo tee /run/lnx-checkpoint-memory >/dev/null; echo ready",
    ]);
    assertEq(write.stdout, "ready", "checkpoint source write");
    const checkpoint = await lnxCommand(ctx.instance, ["checkpoint", "-m", "named-before"]);
    assertEq(checkpoint.stdout, "named-before", "named checkpoint label");
    assertFile(join(ctx.imageDir, "checkpoints"), "checkpoint directory");
    const list = await run([ctx.lnxBin, "--instance", ctx.instance, "checkpoints"]);
    assertContains(list.stdout, "named-before", "checkpoint listed");
  });

  await testStep("fork from named checkpoint is isolated from later source writes", async () => {
    await lnxVm([
      "bash",
      "-lc",
      "printf disk-after | sudo tee /root/lnx-checkpoint-disk >/dev/null; printf memory-after | sudo tee /run/lnx-checkpoint-memory >/dev/null",
    ]);
    assertEq(
      (await lnxCommand(ctx.instance, ["fork", "--checkpoint", "named-before", forkA])).stdout,
      forkA,
      "fork name",
    );
    const forkRead = await lnxCommand(forkA, [
      "bash",
      "-lc",
      'printf "%s/%s" "$(sudo cat /root/lnx-checkpoint-disk)" "$(sudo cat /run/lnx-checkpoint-memory)"',
    ]);
    assertEq(forkRead.stdout, "disk-before/memory-before", "named fork restored checkpoint state");
    const sourceRead = await lnxVm([
      "bash",
      "-lc",
      'printf "%s/%s" "$(sudo cat /root/lnx-checkpoint-disk)" "$(sudo cat /run/lnx-checkpoint-memory)"',
    ]);
    assertEq(sourceRead.stdout, "disk-after/memory-after", "source kept later state");
  });

  await testStep("fork without checkpoint snapshots current VM", async () => {
    await lnxVm([
      "bash",
      "-lc",
      "printf current-disk | sudo tee /root/lnx-checkpoint-disk >/dev/null; printf current-memory | sudo tee /run/lnx-checkpoint-memory >/dev/null",
    ]);
    assertEq((await lnxCommand(ctx.instance, ["fork", forkB])).stdout, forkB, "implicit fork name");
    const forkRead = await lnxCommand(forkB, [
      "bash",
      "-lc",
      'printf "%s/%s" "$(sudo cat /root/lnx-checkpoint-disk)" "$(sudo cat /run/lnx-checkpoint-memory)"',
    ]);
    assertEq(forkRead.stdout, "current-disk/current-memory", "implicit fork restored current state");
  });

  await testStep("duplicate fork destination fails without overwriting", async () => {
    const failed = await lnxCommand(ctx.instance, ["fork", "--checkpoint", "named-before", forkA], {
      check: false,
    });
    assertEq(failed.status === 0, false, "duplicate fork rejected");
    assertContains(failed.stderr, "destination rootfs already exists", "duplicate fork error");
  });
} finally {
  await cleanupContext(ctx);
  await cleanupForks();
}
