import {
  assertEq,
  cleanupContext,
  defaultContext,
  lnx,
  prepareContext,
  testStep,
  waitForOwnerExit,
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
} finally {
  await cleanupContext(ctx);
}
