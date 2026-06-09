import { cp, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { assertContains, assertEq, cleanupContext, defaultContext, lnx, prepareContext, run, testStep } from "./lib";

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
    const restored = await lnx(ctx, [
      "bash",
      "-lc",
      'printf "%s/%s" "$(sudo cat /root/compat-disk)" "$(sudo cat /run/compat-memory)"',
    ]);
    assertEq(restored.stdout, "disk-from-snapshot/memory-from-snapshot", "baseline restores memory and disk");
  });

  await testStep("mismatched snapshot header skips memory restore clearly", async () => {
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
} finally {
  await cleanupContext(ctx);
}
