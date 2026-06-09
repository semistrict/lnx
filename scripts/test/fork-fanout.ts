import { join } from "node:path";
import { cleanupContext, cleanupInstance, defaultContext, lnx, prepareContext, run, assertEq, testStep } from "./lib";

const ctx = defaultContext("fork-fanout");
const forks = Array.from({ length: Number(Bun.env.LNX_FANOUT_COUNT ?? "5") }, (_, i) => `${ctx.instance}-fork-${i}`);

try {
  await prepareContext(ctx);
  for (const fork of forks) await cleanupInstance(ctx, fork);

  await testStep("create named checkpoint", async () => {
    await lnx(ctx, [
      "--no-snapshot-restore",
      "bash",
      "-lc",
      "printf base >/root/fanout-marker; printf base-memory >/run/fanout-marker",
    ]);
    assertEq((await run([ctx.lnxBin, "--instance", ctx.instance, "checkpoint", "-m", "fanout-base"])).stdout, "fanout-base", "checkpoint name");
  });

  await testStep("many forks restore independently", async () => {
    await Promise.all(forks.map((fork) => run([ctx.lnxBin, "--instance", ctx.instance, "fork", "--checkpoint", "fanout-base", fork])));
    await Promise.all(
      forks.map(async (fork, i) => {
        const read = await run([
          ctx.lnxBin,
          "--instance",
          fork,
          "bash",
          "-lc",
          'printf "%s/%s" "$(cat /root/fanout-marker)" "$(cat /run/fanout-marker)"',
        ]);
        assertEq(read.stdout, "base/base-memory", `fork ${i} restored base`);
        await run([ctx.lnxBin, "--instance", fork, "bash", "-lc", `printf fork-${i} >/root/fanout-marker`]);
      }),
    );
    const source = await lnx(ctx, ["cat", "/root/fanout-marker"]);
    assertEq(source.stdout, "base", "source checkpoint clone not mutated by forks");
  });
} finally {
  await cleanupContext(ctx);
  for (const fork of forks) await cleanupInstance(ctx, fork);
}
