import { cp, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { assertContains, assertEq, cleanupContext, defaultContext, lnx, prepareContext, run, testStep, waitForVmSuspend } from "./lib";

Bun.env.LNX_BROKER_IDLE_TTL_MS ??= "500";

const ctx = defaultContext("snapshot-compat");
const badSnapshot = join(ctx.tmpdir, "bad-memory-snapshot");
const corruptSectionSnapshot = join(ctx.tmpdir, "corrupt-section-snapshot");
const legacyCacheSnapshot = join(ctx.tmpdir, "legacy-cache-snapshot");
const shareMismatchSnapshot = join(ctx.tmpdir, "share-mismatch-snapshot");

try {
  await prepareContext(ctx);

  await testStep("create restorable baseline", async () => {
    await lnx(ctx, [
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

  await testStep("mismatched snapshot header rejects memory restore clearly", async () => {
    await waitForVmSuspend(ctx);
    await cp(join(ctx.snapshotDir, "latest"), badSnapshot, { recursive: true });
    const vmstatePath = join(badSnapshot, "vmstate.bin");
    const header = Buffer.from(await readFile(vmstatePath));
    header.writeUInt32LE(99, 32);
    await writeFile(vmstatePath, header);

    const failure = await lnx(ctx, [
      "--snapshot",
      badSnapshot,
      "bash",
      "-lc",
      "echo should-not-run",
    ], {
      check: false,
    });
    if (failure.status === 0) {
      throw new Error("config-mismatched snapshot restore succeeded unexpectedly");
    }
    assertContains(failure.stderr, "snapshot VM config mismatch", "config mismatch rejected");
  });

  await testStep("legacy host-share cache policy stamp rejects memory restore clearly", async () => {
    await waitForVmSuspend(ctx);
    await cp(join(ctx.snapshotDir, "latest"), legacyCacheSnapshot, { recursive: true });
    const stampPath = join(legacyCacheSnapshot, "shares.stamp");
    const currentStamp = await readFile(stampPath, "utf8");
    await writeFile(stampPath, currentStamp.replace(/^host-share-cache=.*\n/, ""));

    const failure = await lnx(ctx, [
      "--snapshot",
      legacyCacheSnapshot,
      "bash",
      "-lc",
      "echo should-not-run",
    ], {
      timeoutMs: 240_000,
      check: false,
    });
    if (failure.status === 0) {
      throw new Error("legacy-cache snapshot restore succeeded unexpectedly");
    }
    assertContains(
      failure.stderr,
      "snapshot host-share/network stamp is incompatible (host_share_cache_policy:",
      "legacy cache policy rejected",
    );
  });

  await testStep("corrupt snapshot section fails hard", async () => {
    // Flip the last byte of vmstate.bin: the header still parses, so the
    // pre-flight accepts the snapshot, but the section hash check refuses it
    // at restore time.
    await waitForVmSuspend(ctx);
    await cp(join(ctx.snapshotDir, "latest"), corruptSectionSnapshot, { recursive: true });
    const vmstatePath = join(corruptSectionSnapshot, "vmstate.bin");
    const vmstate = Buffer.from(await readFile(vmstatePath));
    vmstate[vmstate.length - 1] ^= 0xff;
    await writeFile(vmstatePath, vmstate);

    const failure = await lnx(ctx, [
      "--snapshot",
      corruptSectionSnapshot,
      "bash",
      "-lc",
      "echo should-not-run",
    ], {
      timeoutMs: 240_000,
      check: false,
    });
    if (failure.status === 0) {
      throw new Error("corrupt snapshot restore succeeded unexpectedly");
    }
    assertContains(
      failure.stderr,
      "lnx VM owner exited with exit status: 86 before the broker came up",
      "restore refusal fails hard",
    );
    const log = await run(["bash", "-lc", `cat ${join(ctx.runDir, "lnx.log")}`]);
    assertContains(
      log.stdout,
      "owner.start.restore_failed",
      "restore refusal logged",
    );
  });

  await testStep("share root drift rejects restore", async () => {
    await waitForVmSuspend(ctx);
    await cp(join(ctx.snapshotDir, "latest"), shareMismatchSnapshot, { recursive: true });
    const stampPath = join(shareMismatchSnapshot, "shares.stamp");
    const currentStamp = await readFile(stampPath, "utf8");
    await writeFile(stampPath, currentStamp.replace(/^home=.*\n/m, "home=/Users/lnx-share-drift\n"));

    const drifted = await lnx(ctx, [
      "--snapshot",
      shareMismatchSnapshot,
      "bash",
      "-lc",
      "echo should-not-run",
    ], {
      timeoutMs: 240_000,
      check: false,
    });
    if (drifted.status === 0) {
      throw new Error("share-drifted snapshot restore succeeded unexpectedly");
    }
    assertContains(
      drifted.stderr,
      "snapshot host-share/network stamp is incompatible (share_mismatch:",
      "share root drift rejects the restore",
    );
  });
} finally {
  await cleanupContext(ctx);
}
