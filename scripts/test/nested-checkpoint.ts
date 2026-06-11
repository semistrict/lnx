import { readdir, readFile } from "node:fs/promises";
import { join } from "node:path";
import {
  assertEq,
  cleanupContext,
  defaultContext,
  lnx,
  prepareContext,
  run,
  testStep,
  waitForOwnerExit,
} from "./lib";

const ctx = defaultContext("nested-checkpoint");
const checkpointName = "nested-before";
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

async function checkpointPathByName(name: string): Promise<string> {
  for (const entry of await readdir(join(ctx.imageDir, "checkpoints"), { withFileTypes: true })) {
    if (!entry.isDirectory()) {
      continue;
    }
    const path = join(ctx.imageDir, "checkpoints", entry.name);
    const meta = await readFile(join(path, "checkpoint.meta"), "utf8");
    if (meta.split("\n").includes(`name=${name}`)) {
      return path;
    }
  }
  throw new Error(`checkpoint not found: ${name}`);
}

try {
  await prepareContext(ctx);

  await testStep("named checkpoint restores disk and memory in nested Linux", async () => {
    const write = await lnxVm([
      "--no-snapshot-restore",
      "bash",
      "-lc",
      "printf nested-disk-before | sudo tee /root/lnx-nested-checkpoint-disk >/dev/null; printf nested-memory-before | sudo tee /run/lnx-nested-checkpoint-memory >/dev/null; echo ready",
    ]);
    assertEq(write.stdout, "ready", "checkpoint source write");

    const checkpoint = await lnxCommand(ctx.instance, ["checkpoint", "-m", checkpointName]);
    assertEq(checkpoint.stdout, checkpointName, "checkpoint name");
    const checkpointPath = await checkpointPathByName(checkpointName);

    await lnxVm([
      "bash",
      "-lc",
      "printf nested-disk-after | sudo tee /root/lnx-nested-checkpoint-disk >/dev/null; printf nested-memory-after | sudo tee /run/lnx-nested-checkpoint-memory >/dev/null",
    ]);

    const source = await lnxVm([
      "bash",
      "-lc",
      'printf "%s/%s" "$(sudo cat /root/lnx-nested-checkpoint-disk)" "$(sudo cat /run/lnx-nested-checkpoint-memory)"',
    ]);
    assertEq(source.stdout, "nested-disk-after/nested-memory-after", "source kept later state");

    await waitForOwnerExit(ctx, 120_000);

    const restored = await lnxCommand(ctx.instance, [
      "--rootfs",
      join(checkpointPath, "rootfs.ext4"),
      "--snapshot",
      checkpointPath,
      "bash",
      "-lc",
      'printf "%s/%s" "$(sudo cat /root/lnx-nested-checkpoint-disk)" "$(sudo cat /run/lnx-nested-checkpoint-memory)"',
    ]);
    assertEq(
      restored.stdout,
      "nested-disk-before/nested-memory-before",
      "checkpoint restored disk and memory",
    );
  });
} finally {
  await cleanupContext(ctx);
}
