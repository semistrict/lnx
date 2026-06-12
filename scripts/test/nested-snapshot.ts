import { readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import {
  assertContains,
  assertEq,
  cleanupContext,
  defaultContext,
  lnx,
  prepareContext,
  testStep,
  waitForOwnerExit,
  waitForVmSuspend,
} from "./lib";

const ctx = defaultContext("nested-snapshot");
const vmArgs = [
  ...(Bun.env.LNX_TEST_CPUS ? ["--cpus", Bun.env.LNX_TEST_CPUS] : []),
  ...(Bun.env.LNX_TEST_MEMORY_MIB ? ["--memory-mib", Bun.env.LNX_TEST_MEMORY_MIB] : []),
];

function lnxVm(args: string[], options: Parameters<typeof lnx>[1] = {}) {
  return lnx(ctx, [...vmArgs, ...args], options);
}

try {
  await prepareContext(ctx);

  await testStep("Linux-host snapshot restores disk and memory state", async () => {
    const write = await lnxVm([
      "--no-snapshot-restore",
      "bash",
      "-lc",
      "printf nested-disk | sudo tee /root/lnx-nested-snapshot-disk >/dev/null; printf nested-memory | sudo tee /run/lnx-nested-snapshot-memory >/dev/null; echo ready",
    ]);
    assertEq(write.stdout, "ready", "snapshot source write");

    const restored = await lnxVm([
      "bash",
      "-lc",
      'printf "%s/%s" "$(sudo cat /root/lnx-nested-snapshot-disk)" "$(sudo cat /run/lnx-nested-snapshot-memory)"',
    ]);
    assertEq(restored.stdout, "nested-disk/nested-memory", "snapshot restored disk and memory");

    const explicit = await lnxVm([
      "--snapshot",
      `${ctx.snapshotDir}/latest`,
      "bash",
      "-lc",
      'printf "%s/%s" "$(sudo cat /root/lnx-nested-snapshot-disk)" "$(sudo cat /run/lnx-nested-snapshot-memory)"',
    ]);
    assertEq(explicit.stdout, "nested-disk/nested-memory", "explicit snapshot restored disk and memory");
  });

  await testStep("restored VM still delivers timer interrupts", async () => {
    await waitForOwnerExit(ctx, 120_000);
    const slept = await lnxVm(["bash", "-lc", "sleep 0.2 && echo slept"], { timeoutMs: 60_000 });
    assertEq(slept.stdout, "slept", "sleep completed in restored VM");
  });

  await testStep("corrupt snapshot section falls back to a cold boot", async () => {
    // Flip the last byte of vmstate.bin: the header still parses, so the
    // pre-flight accepts the snapshot, but the section hash check refuses it
    // at restore time and the client must respawn the owner cold.
    await waitForVmSuspend(ctx, 120_000);
    const vmstatePath = join(ctx.snapshotDir, "latest", "vmstate.bin");
    const vmstate = Buffer.from(await readFile(vmstatePath));
    vmstate[vmstate.length - 1] ^= 0xff;
    await writeFile(vmstatePath, vmstate);

    const fallback = await lnxVm(["bash", "-lc", "echo cold-fallback-ok"], {
      timeoutMs: 240_000,
    });
    assertEq(fallback.stdout, "cold-fallback-ok", "exec succeeds after restore refusal");
    const log = await Bun.file(`${ctx.runDir}/lnx.log`).text();
    assertContains(
      log,
      "snapshot.restore.skipped reason=start_failed retry=cold_boot",
      "restore refusal logged and retried cold",
    );
  });

  await testStep("share root drift skips restore and boots cold", async () => {
    // Running from the other side of the $HOME boundary changes the host
    // share roots, so the pre-flight must skip the restore and boot cold; a
    // restored guest would keep mounts backed by the old share roots.
    await waitForVmSuspend(ctx, 120_000);
    const home = Bun.env.HOME ?? "/";
    const insideHome = `${process.cwd()}/`.startsWith(`${home}/`);
    const otherSide = insideHome ? "/tmp" : home;
    // A working exec proves the cwd chdir succeeded: the agent fails the
    // exec outright when the share backing the cwd is missing.
    const drifted = await lnxVm(["bash", "-lc", "echo share-drift-ok"], {
      cwd: otherSide,
      timeoutMs: 240_000,
    });
    assertEq(drifted.stdout, "share-drift-ok", "exec runs in the drifted cwd");
    const log = await Bun.file(`${ctx.runDir}/lnx.log`).text();
    assertContains(
      log,
      "snapshot.restore.skipped reason=share_mismatch",
      "share root drift skips the restore",
    );
  });
} finally {
  await cleanupContext(ctx);
}
