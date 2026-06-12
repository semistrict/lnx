import { cp, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { assertContains, assertEq, cleanupContext, defaultContext, lnx, prepareContext, run, testStep, waitForVmSuspend } from "./lib";

Bun.env.LNX_BROKER_IDLE_TTL_MS ??= "500";

const ctx = defaultContext("snapshot-compat");
const badSnapshot = join(ctx.tmpdir, "bad-memory-snapshot");

try {
  await prepareContext(ctx);

  await testStep("create restorable baseline", async () => {
    await lnx(ctx, [
      "--no-snapshot-restore",
      "bash",
      "-lc",
      "printf disk-from-snapshot | sudo tee /root/compat-disk >/dev/null; printf memory-from-snapshot | sudo tee /run/compat-memory >/dev/null",
    ]);
    await waitForVmSuspend(ctx);
    const restored = await lnx(ctx, [
      "bash",
      "-lc",
      'printf "%s/%s" "$(sudo cat /root/compat-disk)" "$(sudo cat /run/compat-memory)"',
    ]);
    assertEq(restored.stdout, "disk-from-snapshot/memory-from-snapshot", "baseline restores memory and disk");
  });

  await testStep("mismatched snapshot header skips memory restore clearly", async () => {
    await waitForVmSuspend(ctx);
    await cp(join(ctx.snapshotDir, "latest"), badSnapshot, { recursive: true });
    const vmstatePath = join(badSnapshot, "vmstate.bin");
    const header = Buffer.from(await readFile(vmstatePath));
    header.writeUInt32LE(99, 32);
    await writeFile(vmstatePath, header);

    const result = await lnx(ctx, [
      "--snapshot",
      badSnapshot,
      "bash",
      "-lc",
      'disk="$(sudo cat /root/compat-disk 2>/dev/null || true)"; memory="$(sudo cat /run/compat-memory 2>/dev/null || true)"; printf "%s/%s" "$disk" "$memory"',
    ]);
    assertEq(result.stdout, "disk-from-snapshot/", "mismatch skips memory restore while using requested snapshot rootfs");
    const log = await run(["bash", "-lc", `cat ${join(ctx.runDir, "lnx.log")}`]);
    assertContains(log.stdout, "snapshot.restore.skipped reason=config_mismatch", "config mismatch logged");
  });

  await testStep("corrupt snapshot section falls back to a cold boot", async () => {
    // Flip the last byte of vmstate.bin: the header still parses, so the
    // pre-flight accepts the snapshot, but the section hash check refuses it
    // at restore time and the client must respawn the owner cold.
    await waitForVmSuspend(ctx);
    const vmstatePath = join(ctx.snapshotDir, "latest", "vmstate.bin");
    const vmstate = Buffer.from(await readFile(vmstatePath));
    vmstate[vmstate.length - 1] ^= 0xff;
    await writeFile(vmstatePath, vmstate);

    const fallback = await lnx(ctx, ["bash", "-lc", "echo cold-fallback-ok"], {
      timeoutMs: 240_000,
    });
    assertEq(fallback.stdout, "cold-fallback-ok", "exec succeeds after restore refusal");
    const log = await run(["bash", "-lc", `cat ${join(ctx.runDir, "lnx.log")}`]);
    assertContains(
      log.stdout,
      "snapshot.restore.skipped reason=start_failed retry=cold_boot",
      "restore refusal logged and retried cold",
    );
  });

  await testStep("share root drift skips restore and boots cold", async () => {
    // The earlier steps captured snapshots from this suite's cwd. Running
    // from the other side of the $HOME boundary changes the host share
    // roots, so the pre-flight must skip the restore and boot cold; a
    // restored guest would keep mounts backed by the old share roots.
    await waitForVmSuspend(ctx);
    const home = Bun.env.HOME ?? "/";
    const insideHome = `${process.cwd()}/`.startsWith(`${home}/`);
    const otherSide = insideHome ? "/tmp" : home;
    // A working exec proves the cwd chdir succeeded: the agent fails the
    // exec outright when the share backing the cwd is missing.
    const drifted = await lnx(ctx, ["bash", "-lc", "echo share-drift-ok"], {
      cwd: otherSide,
      timeoutMs: 240_000,
    });
    assertEq(drifted.stdout, "share-drift-ok", "exec runs in the drifted cwd");
    const log = await run(["bash", "-lc", `cat ${join(ctx.runDir, "lnx.log")}`]);
    assertContains(
      log.stdout,
      "snapshot.restore.skipped reason=share_mismatch",
      "share root drift skips the restore",
    );
  });
} finally {
  await cleanupContext(ctx);
}
