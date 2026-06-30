import {
  existsSync } from "node:fs";
import { join } from "node:path";
import { assertEq,
  cleanupContext,
  defaultContext,
  prepareContext,
  sleep,
  spawn,
  testStep,
  type LnxCliOptions,
} from "./lib";

const ctx = defaultContext("broker-recovery");
const vmArgs = [
  ...(Bun.env.LNX_TEST_CPUS ? ["--cpus", Bun.env.LNX_TEST_CPUS] : []),
  ...(Bun.env.LNX_TEST_MEMORY_MIB ? ["--memory-mib", Bun.env.LNX_TEST_MEMORY_MIB] : []),
];

function lnxVm(args: string[], options: LnxCliOptions = {}) {
  return ctx.vm.cli([...vmArgs, ...args], options);
}

function spawnLnxVm(args: string[], options: Parameters<typeof spawn>[1] = {}) {
  return spawn([ctx.lnxBin, "--instance", ctx.instance, ...vmArgs, ...args], options);
}

try {
  await prepareContext(ctx);

  await testStep("killed owner leaves next command recoverable", async () => {
    const proc = spawnLnxVm(["bash", "-lc", "echo owner-ready; sleep 60"], {
      stdout: "pipe",
      stderr: "pipe",
    });
    for (let i = 0; i < 100 && !existsSync(join(ctx.runDir, "broker.sock")); i++) {
      await sleep(100);
    }
    proc.kill("SIGKILL");
    await proc.exited.catch(() => {});
    const recovered = await lnxVm(["echo", "recovered"], { timeoutMs: 180_000 });
    assertEq(recovered.stdout, "recovered", "recovered after killed owner");
  });
} finally {
  await cleanupContext(ctx);
}
