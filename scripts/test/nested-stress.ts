import {
  assertEq,
  cleanupContext,
  defaultContext,
  prepareContext,
  testStep,
  type LnxCliOptions,
} from "./lib";

const ctx = defaultContext("nested-stress");
const vmArgs = [
  ...(Bun.env.LNX_TEST_CPUS ? ["--cpus", Bun.env.LNX_TEST_CPUS] : []),
  ...(Bun.env.LNX_TEST_MEMORY_MIB ? ["--memory-mib", Bun.env.LNX_TEST_MEMORY_MIB] : []),
];

function lnxVm(args: string[], options: LnxCliOptions = {}) {
  return ctx.vm.cli([...vmArgs, ...args], options);
}

try {
  await prepareContext(ctx);

  await testStep("warm nested-host VM", async () => {
    assertEq((await lnxVm(["echo", "warm"])).stdout, "warm", "warm exec");
  });

  await testStep("parallel non-pty channels on a Linux host", async () => {
    const count = Number(Bun.env.LNX_STRESS_COUNT ?? "20");
    const concurrency = Number(Bun.env.LNX_STRESS_CONCURRENCY ?? "8");
    const pending = Array.from({ length: count }, (_, index) => index);
    const results: string[] = [];
    async function worker() {
      while (pending.length) {
        const id = pending.shift();
        if (id === undefined) return;
        const delay = id % 7;
        const result = await lnxVm(["bash", "-lc", `echo start-${id}; sleep 0.${delay}; echo end-${id}`]);
        results[id] = result.stdout;
      }
    }
    await Promise.all(Array.from({ length: concurrency }, () => worker()));
    for (let id = 0; id < count; id++) {
      assertEq(results[id], `start-${id}\nend-${id}`, `parallel output ${id}`);
    }
  });

  await testStep("mixed stdin and exit status on a Linux host", async () => {
    const [cat, fail, slow] = await Promise.all([
      lnxVm(["cat"], { stdin: "pipe-ok" }),
      lnxVm(["bash", "-lc", "exit 33"], { check: false }),
      lnxVm(["bash", "-lc", "sleep 1; echo slow-ok"]),
    ]);
    assertEq(cat.stdout, "pipe-ok", "parallel stdin");
    assertEq(fail.status, 33, "parallel failing status");
    assertEq(slow.stdout, "slow-ok", "parallel slow output");
  });
} finally {
  await cleanupContext(ctx);
}
