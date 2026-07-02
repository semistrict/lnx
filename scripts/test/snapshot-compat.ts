import {
  cp,
  readFile,
  writeFile } from "node:fs/promises";
import { join } from "node:path";
import { assertContains,
  assertEq,
  cleanupContext,
  defaultContext,
  prepareContext,
  run,
  testStep,
  waitForVmSuspend,
} from "./lib";

Bun.env.LNX_BROKER_IDLE_TTL_MS ??= "500";

const ctx = defaultContext("snapshot-compat");
const badSnapshot = join(ctx.tmpdir, "bad-memory-snapshot");
const corruptSectionSnapshot = join(ctx.tmpdir, "corrupt-section-snapshot");
const missingLaunchSnapshot = join(ctx.tmpdir, "missing-launch-snapshot");
const shareMismatchSnapshot = join(ctx.tmpdir, "share-mismatch-snapshot");

try {
  await prepareContext(ctx);

  await testStep("create restorable baseline", async () => {
    await ctx.vm.cli([
      "bash",
      "-lc",
      "printf disk-from-snapshot | sudo tee /root/compat-disk >/dev/null; printf memory-from-snapshot | sudo tee /run/compat-memory >/dev/null",
    ]);
    await waitForVmSuspend(ctx);
    const restored = await ctx.vm.cli([
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

    const failure = await ctx.vm.cli([
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

  await testStep("snapshot without launch metadata rejects memory restore clearly", async () => {
    await waitForVmSuspend(ctx);
    await cp(join(ctx.snapshotDir, "latest"), missingLaunchSnapshot, { recursive: true });
    await Bun.file(join(missingLaunchSnapshot, "launch.json")).delete();

    const failure = await ctx.vm.cli([
      "--snapshot",
      missingLaunchSnapshot,
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
      "snapshot launch metadata is incompatible (launch_metadata: snapshot has no launch.json",
      "missing launch metadata rejected",
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

    const failure = await ctx.vm.cli([
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
      "VM memory snapshot restore was refused before the broker came up",
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
    const metadataPath = join(shareMismatchSnapshot, "launch.json");
    const launchMetadata = JSON.parse(await readFile(metadataPath, "utf8"));
    launchMetadata.shares.host_home = "/Users/lnx-share-drift";
    await writeFile(metadataPath, JSON.stringify(launchMetadata, null, 2) + "\n");

    const drifted = await ctx.vm.cli([
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
      "snapshot launch metadata is incompatible (share_mismatch:",
      "share root drift rejects the restore",
    );
  });
} finally {
  await cleanupContext(ctx);
}
