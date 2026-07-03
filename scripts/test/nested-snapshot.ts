import {
  cp,
  readFile,
  writeFile } from "node:fs/promises";
import { join } from "node:path";
import {
  assertContains,
  assertEq,
  cleanupContext,
  defaultContext,
  prepareContext,
  testStep,
  waitForOwnerExit,
  waitForVmSuspend,
  type LnxCliOptions,
} from "./lib";

const ctx = defaultContext("nested-snapshot");
const corruptSectionSnapshot = join(ctx.tmpdir, "corrupt-section-snapshot");
const vmArgs = [
  ...(Bun.env.LNX_TEST_CPUS ? ["--cpus", Bun.env.LNX_TEST_CPUS] : []),
  ...(Bun.env.LNX_TEST_MEMORY_MIB ? ["--memory-mib", Bun.env.LNX_TEST_MEMORY_MIB] : []),
];

function lnxVm(args: string[], options: LnxCliOptions = {}) {
  return ctx.vm.cli([...vmArgs, ...args], options);
}

try {
  await prepareContext(ctx);

  await testStep("Linux-host snapshot restores disk and memory state", async () => {
    const write = await lnxVm([
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

  await testStep("corrupt snapshot section fails hard", async () => {
    // Flip the last byte of vmstate.bin: the header still parses, so the
    // pre-flight accepts the snapshot, but the section hash check refuses it
    // at restore time.
    await waitForVmSuspend(ctx, 120_000);
    await cp(join(ctx.snapshotDir, "latest"), corruptSectionSnapshot, { recursive: true });
    const vmstatePath = join(corruptSectionSnapshot, "vmstate.bin");
    const vmstate = Buffer.from(await readFile(vmstatePath));
    vmstate[vmstate.length - 1] ^= 0xff;
    await writeFile(vmstatePath, vmstate);

    const failure = await lnxVm([
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
    const log = await Bun.file(`${ctx.runDir}/lnx.log`).text();
    assertContains(
      log,
      "owner.start.restore_failed",
      "restore refusal logged",
    );
  });

  await testStep("share root drift rejects restore", async () => {
    // Running from the other side of the $HOME boundary changes the host
    // share roots, so the pre-flight must refuse the restore; a
    // restored guest would keep mounts backed by the old share roots.
    await waitForVmSuspend(ctx, 120_000);
    const home = Bun.env.HOME ?? "/";
    const insideHome = `${process.cwd()}/`.startsWith(`${home}/`);
    const otherSide = insideHome ? "/tmp" : home;
    // A working exec proves the cwd chdir succeeded: the agent fails the
    // exec outright when the share backing the cwd is missing.
    const drifted = await lnxVm(["bash", "-lc", "echo should-not-run"], {
      cwd: otherSide,
      timeoutMs: 240_000,
      check: false,
    });
    if (drifted.status === 0) {
      throw new Error("share-drifted snapshot restore succeeded unexpectedly");
    }
    assertContains(
      drifted.stderr,
      "snapshot launch metadata is incompatible (share_mismatch)",
      "share root drift rejects the restore",
    );
  });
} finally {
  await cleanupContext(ctx);
}
