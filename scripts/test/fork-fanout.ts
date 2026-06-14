import { cleanupContext, cleanupInstance, defaultContext, lnx, prepareContext, run, assertEq, testStep } from "./lib";

const ctx = defaultContext("fork-fanout");
const forks = Array.from({ length: Number(Bun.env.LNX_FANOUT_COUNT ?? "5") }, (_, i) => `${ctx.instance}-fork-${i}`);

try {
  await prepareContext(ctx);
  for (const fork of forks) await cleanupInstance(ctx, fork);

  await testStep("create named checkpoint", async () => {
    await lnx(ctx, [
      "bash",
      "-lc",
      "printf base | sudo tee /root/fanout-marker >/dev/null; printf base-memory | sudo tee /run/fanout-marker >/dev/null",
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
          'printf "%s/%s" "$(sudo cat /root/fanout-marker)" "$(sudo cat /run/fanout-marker)"',
        ]);
        assertEq(read.stdout, "base/base-memory", `fork ${i} restored base`);
        await run([ctx.lnxBin, "--instance", fork, "bash", "-lc", `printf fork-${i} | sudo tee /root/fanout-marker >/dev/null`]);
      }),
    );
    const entropyReads = await Promise.all(
      forks.map((fork) =>
        run([
          ctx.lnxBin,
          "--instance",
          fork,
          "bash",
          "-lc",
          String.raw`test -s /run/lnx-vmstate-reseed
python3 - <<'PY'
import hashlib
import os

print(hashlib.sha256(os.getrandom(64) + open("/dev/urandom", "rb").read(64)).hexdigest())
PY`,
        ]),
      ),
    );
    assertEq(
      new Set(entropyReads.map((read) => read.stdout)).size,
      forks.length,
      "fork restore entropy probes are unique",
    );
    const source = await lnx(ctx, ["sudo", "cat", "/root/fanout-marker"]);
    assertEq(source.stdout, "base", "source checkpoint clone not mutated by forks");
  });
} finally {
  await cleanupContext(ctx);
  for (const fork of forks) await cleanupInstance(ctx, fork);
}
