import { assertEq, cleanupContext, defaultContext, lnx, prepareContext, spawnLnx, sleep, testStep } from "./lib";

const ctx = defaultContext("client-chaos");

try {
  await prepareContext(ctx);

  await testStep("warm up instance", async () => {
    const ready = await lnx(ctx, ["echo", "ready"], { timeoutMs: 180_000 });
    assertEq(ready.stdout, "ready", "warmup boot");
  });

  await testStep("disconnecting non-pty client does not poison broker", async () => {
    const proc = spawnLnx(ctx, ["bash", "-lc", "trap 'exit 0' TERM; sleep 60"], {
      stdout: "pipe",
      stderr: "pipe",
    });
    await sleep(1000);
    proc.kill("SIGKILL");
    await proc.exited.catch(() => {});
    assertEq((await lnx(ctx, ["echo", "after-non-pty-disconnect"])).stdout, "after-non-pty-disconnect", "broker usable after non-pty disconnect");
  });

  await testStep("disconnecting pty client does not poison broker", async () => {
    const proc = spawnLnx(ctx, ["bash", "-lc", "trap 'exit 0' TERM; sleep 60"], {
      stdin: "pipe",
      stdout: "pipe",
      stderr: "pipe",
      env: { TERM: "xterm-256color" },
    });
    await sleep(1000);
    proc.kill("SIGKILL");
    await proc.exited.catch(() => {});
    assertEq((await lnx(ctx, ["echo", "after-pty-disconnect"])).stdout, "after-pty-disconnect", "broker usable after pty disconnect");
  });
} finally {
  await cleanupContext(ctx);
}
