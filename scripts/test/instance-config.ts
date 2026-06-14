import { readFile, rm } from "node:fs/promises";
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
  spawnLnx,
  testStep,
  waitForOwnerExit,
  waitForVmSuspend,
} from "./lib";

const ctx = defaultContext("instance-config");

function lnxCommand(args: string[], options: Parameters<typeof run>[1] = {}) {
  return run([ctx.lnxBin, "--instance", ctx.instance, ...args], options);
}

try {
  await prepareContext(ctx);

  await testStep("set persists settings in the descriptor", async () => {
    const set = await lnxCommand(["set", "cpus=1", "memory-mib=2048"]);
    const settings = JSON.parse(set.stdout);
    assertEq(settings.cpus, 1, "set echoes cpus");
    assertEq(settings.memory_mib, 2048, "set echoes memory");
    assertEq(settings.name, ctx.instance, "set records instance name");
    assertFile(join(ctx.imageDir, "lnx.json"), "descriptor file");
    const rejected = await lnxCommand(["set", "color=blue"], { check: false });
    assertEq(rejected.status === 0, false, "unknown keys are rejected");
  });

  await testStep("persisted settings drive the VM", async () => {
    assertEq(
      (await lnx(ctx, ["nproc"])).stdout,
      "1",
      "nproc honors persisted cpus",
    );
    await waitForVmSuspend(ctx);
    const vmstate = await readFile(join(ctx.snapshotDir, "latest", "vmstate.bin"));
    const view = new DataView(vmstate.buffer, vmstate.byteOffset, vmstate.byteLength);
    assertEq(
      Number(view.getBigUint64(16, true)),
      2048 * 1024 * 1024,
      "snapshot header memory matches persisted setting",
    );
    assertEq(view.getUint32(32, true), 1, "snapshot header vcpus match persisted setting");
  });

  await testStep("explicit flags override persisted settings", async () => {
    const mismatch = await lnx(ctx, ["--cpus", "2", "nproc"], { check: false });
    assertEq(mismatch.status === 0, false, "incompatible snapshot rejects restore");
    assertContains(mismatch.stderr, "snapshot VM config mismatch", "snapshot mismatch is explicit");
    await rm(join(ctx.snapshotDir, "latest"), { recursive: true, force: true });
    assertEq(
      (await lnx(ctx, ["--cpus", "2", "nproc"])).stdout,
      "2",
      "explicit cpus override",
    );
  });

  await testStep("inspect sees a running instance", async () => {
    const running = spawnLnx(ctx, ["bash", "-lc", "echo inspect-ready; sleep 30"], {
      stdout: "pipe",
    });
    for await (const chunk of running.stdout) {
      if (new TextDecoder().decode(chunk).includes("inspect-ready")) {
        break;
      }
    }
    const inspect = JSON.parse((await lnxCommand(["inspect"])).stdout);
    assertEq(inspect.state, "running", "inspect running state");
    assertEq(inspect.pids.length > 0, true, "inspect lists owner pid");
    running.kill("SIGKILL");
    await running.exited.catch(() => {});
  });

  await testStep("inspect reports state and configuration", async () => {
    await waitForOwnerExit(ctx, 120_000);
    const inspect = JSON.parse((await lnxCommand(["inspect"])).stdout);
    assertEq(inspect.name, ctx.instance, "inspect name");
    assertEq(inspect.state, "stopped", "inspect state");
    assertEq(inspect.cpus, 1, "inspect effective cpus");
    assertEq(inspect.memory_mib, 2048, "inspect effective memory");
    assertEq(inspect.settings.cpus, 1, "inspect persisted cpus");
    assertEq(inspect.settings.memory_mib, 2048, "inspect persisted memory");
    assertEq(inspect.image, "release:images-v0.4.0", "inspect image source");
    assertEq(inspect.checkpoints, 0, "inspect checkpoint count");
    assertEq(inspect.rootfs, ctx.imageDir + "/rootfs.ext4", "inspect rootfs path");
    assertEq(typeof inspect.created, "string", "inspect created timestamp");
    assertEq(typeof inspect.snapshot.pages_allocated_bytes, "number", "inspect snapshot pages size");
  });

  await testStep("logs prints instance logs", async () => {
    assertContains((await lnxCommand(["logs"])).stdout, "owner.start", "run log content");
    assertContains(
      (await lnxCommand(["logs", "--console"])).stdout,
      "lnx-agent",
      "console log content",
    );
    const missing = await run(
      [ctx.lnxBin, "--instance", `${ctx.instance}-never-started`, "logs"],
      { check: false },
    );
    assertEq(missing.status === 0, false, "logs for unstarted instance fails");
  });
} finally {
  await cleanupContext(ctx);
}
