import { expect } from "bun:test";

export async function runScript(name: string, timeoutMs = 1_800_000) {
  const proc = Bun.spawn(["bun", `scripts/test/${name}.ts`], {
    env: Bun.env,
    stdout: "inherit",
    stderr: "inherit",
  });
  let timeout: Timer | undefined;
  const status = await Promise.race([
    proc.exited,
    new Promise<number>((_, reject) => {
      timeout = setTimeout(() => {
        proc.kill("SIGKILL");
        reject(new Error(`${name} timed out after ${timeoutMs}ms`));
      }, timeoutMs);
    }),
  ]).finally(() => {
    if (timeout) clearTimeout(timeout);
  });
  expect(status).toBe(0);
}
