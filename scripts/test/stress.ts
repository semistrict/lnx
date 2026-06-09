import { assertEq, cleanupContext, defaultContext, lnx, prepareContext, testStep } from "./lib";

const ctx = defaultContext("stress");

try {
  await prepareContext(ctx);

  await testStep("warm VM", async () => {
    assertEq((await lnx(ctx, ["--no-snapshot-restore", "echo", "warm"])).stdout, "warm", "warm exec");
  });

  await testStep("parallel non-pty channels", async () => {
    const count = Number(Bun.env.LNX_STRESS_COUNT ?? "50");
    const concurrency = Number(Bun.env.LNX_STRESS_CONCURRENCY ?? "20");
    const pending = Array.from({ length: count }, (_, index) => index);
    const results: string[] = [];
    async function worker() {
      while (pending.length) {
        const id = pending.shift();
        if (id === undefined) return;
        const delay = id % 7;
        const result = await lnx(ctx, ["bash", "-lc", `echo start-${id}; sleep 0.${delay}; echo end-${id}`]);
        results[id] = result.stdout;
      }
    }
    await Promise.all(Array.from({ length: concurrency }, () => worker()));
    for (let id = 0; id < count; id++) {
      assertEq(results[id], `start-${id}\nend-${id}`, `parallel output ${id}`);
    }
  });

  await testStep("mixed stdin and exit status", async () => {
    const [cat, fail, slow] = await Promise.all([
      lnx(ctx, ["cat"], { stdin: "pipe-ok" }),
      lnx(ctx, ["bash", "-lc", "exit 33"], { check: false }),
      lnx(ctx, ["bash", "-lc", "sleep 1; echo slow-ok"]),
    ]);
    assertEq(cat.stdout, "pipe-ok", "parallel stdin");
    assertEq(fail.status, 33, "parallel failing status");
    assertEq(slow.stdout, "slow-ok", "parallel slow output");
  });

  await testStep("snapshot waits for active channels", async () => {
    const delayed = lnx(ctx, ["bash", "-lc", "sleep 1; echo delayed"]);
    const snapshot = lnx(ctx, ["lnxctl", "snapshot-exit"]);
    assertEq((await delayed).stdout, "delayed", "delayed channel");
    assertEq((await snapshot).status, 0, "snapshot channel");
  });
} finally {
  await cleanupContext(ctx);
}
