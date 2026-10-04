import type { Readable } from "node:stream";
import {
  assertEq,
  cleanupContext,
  defaultContext,
  prepareContext,
  sleep,
  testStep,
  waitForVmSuspend,
} from "./lib";

const ctx = defaultContext("client-chaos");

/** Resolves with everything read once `needle` has appeared on `stream`. */
function waitForOutput(stream: Readable | null, needle: string, timeoutMs = 30_000): Promise<string> {
  if (!stream) throw new Error("process has no output stream");
  return new Promise((resolve, reject) => {
    let seen = "";
    const timer = setTimeout(() => {
      stream.off("data", onData);
      reject(new Error(`timed out waiting for <${needle}>; saw <${seen}>`));
    }, timeoutMs);
    const onData = (chunk: Buffer) => {
      seen += chunk.toString();
      if (seen.includes(needle)) {
        clearTimeout(timer);
        stream.off("data", onData);
        resolve(seen);
      }
    };
    stream.on("data", onData);
  });
}

/**
 * Whether a guest `sleep SECONDS` is gone, polling inside the guest. The
 * `[s]` keeps the probe's own command line from matching.
 */
async function guestSleepGone(seconds: number): Promise<string> {
  const probe = `for i in $(seq 100); do pgrep -f '[s]leep ${seconds}' >/dev/null || { echo gone; exit 0; }; sleep 0.1; done; echo alive`;
  return (await ctx.vm.cli(["bash", "-c", probe])).stdout;
}

try {
  await prepareContext(ctx);

  await testStep("warm up instance", async () => {
    const ready = await ctx.vm.cli(["echo", "ready"], { timeoutMs: 180_000 });
    assertEq(ready.stdout, "ready", "warmup boot");
  });

  await testStep("disconnecting non-pty client does not poison broker", async () => {
    const proc = ctx.vm.spawnCli(["bash", "-lc", "trap 'exit 0' TERM; sleep 60"], {
      stdout: "pipe",
      stderr: "pipe",
    });
    await sleep(1000);
    proc.kill("SIGKILL");
    await proc.exited.catch(() => {});
    assertEq((await ctx.vm.cli(["echo", "after-non-pty-disconnect"])).stdout, "after-non-pty-disconnect", "broker usable after non-pty disconnect");
  });

  await testStep("disconnecting pty client does not poison broker", async () => {
    const proc = ctx.vm.spawnCli(["bash", "-lc", "trap 'exit 0' TERM; sleep 60"], {
      stdin: "pipe",
      stdout: "pipe",
      stderr: "pipe",
      env: { TERM: "xterm-256color" },
    });
    await sleep(1000);
    proc.kill("SIGKILL");
    await proc.exited.catch(() => {});
    assertEq((await ctx.vm.cli(["echo", "after-pty-disconnect"])).stdout, "after-pty-disconnect", "broker usable after pty disconnect");
  });

  await testStep("stdin streams to the guest as it arrives", async () => {
    const proc = ctx.vm.spawnCli(["bash", "-c", "read first; echo got:$first; read second; echo done:$second"], {
      stdin: "pipe",
      stdout: "pipe",
      stderr: "pipe",
    });
    proc.stdin?.write("a\n");
    await waitForOutput(proc.stdout, "got:a");
    const rest = waitForOutput(proc.stdout, "done:b");
    proc.stdin?.end("b\n");
    await rest;
    assertEq(await proc.exited, 0, "streamed stdin exit status");
  });

  for (const signal of ["SIGTERM", "SIGHUP", "SIGKILL"] as const) {
    await testStep(`${signal} to the client ends the guest command`, async () => {
      const marker = signal === "SIGTERM" ? 7771 : signal === "SIGHUP" ? 7772 : 7773;
      const proc = ctx.vm.spawnCli(["bash", "-c", `echo started; sleep ${marker}`], {
        stdout: "pipe",
        stderr: "pipe",
      });
      await waitForOutput(proc.stdout, "started");
      proc.kill(signal);
      const status = await proc.exited;
      if (signal === "SIGTERM") assertEq(status, 143, "client exit status after SIGTERM");
      if (signal === "SIGHUP") assertEq(status, 129, "client exit status after SIGHUP");
      assertEq(await guestSleepGone(marker), "gone", `guest command after ${signal}`);
    });
  }

  await testStep("a command that ignores the hangup is killed after a grace period", async () => {
    const proc = ctx.vm.spawnCli(["bash", "-c", "trap '' HUP TERM; echo started; sleep 7775"], {
      stdout: "pipe",
      stderr: "pipe",
    });
    await waitForOutput(proc.stdout, "started");
    proc.kill("SIGKILL");
    await proc.exited;
    assertEq(await guestSleepGone(7775), "gone", "stubborn guest command");
  });

  await testStep("an instance whose client was killed goes idle", async () => {
    const proc = ctx.vm.spawnCli(["bash", "-c", "echo started; sleep 7774"], {
      stdout: "pipe",
      stderr: "pipe",
    });
    await waitForOutput(proc.stdout, "started");
    proc.kill("SIGKILL");
    await proc.exited;
    await waitForVmSuspend(ctx);
  });
} finally {
  await cleanupContext(ctx);
}
