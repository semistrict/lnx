import { existsSync } from "node:fs";
import { join } from "node:path";
import { assertEq, cleanupContext, defaultContext, lnx, prepareContext, spawnLnx, sleep, testStep } from "./lib";

const ctx = defaultContext("broker-recovery");

try {
  await prepareContext(ctx);

  await testStep("killed owner leaves next command recoverable", async () => {
    const proc = spawnLnx(ctx, ["--no-snapshot-restore", "bash", "-lc", "echo owner-ready; sleep 60"], {
      stdout: "pipe",
      stderr: "pipe",
    });
    for (let i = 0; i < 100 && !existsSync(join(ctx.runDir, "broker.sock")); i++) {
      await sleep(100);
    }
    proc.kill("SIGKILL");
    await proc.exited.catch(() => {});
    const recovered = await lnx(ctx, ["echo", "recovered"], { timeoutMs: 180_000 });
    assertEq(recovered.stdout, "recovered", "recovered after killed owner");
  });
} finally {
  await cleanupContext(ctx);
}
