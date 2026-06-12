import { mkdir, rm, writeFile, readFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { tmpdir } from "node:os";

export type ExecResult = {
  status: number;
  stdout: string;
  stderr: string;
};

export type TestContext = {
  repoRoot: string;
  lnxBin: string;
  instance: string;
  base: string;
  tmpdir: string;
  imageDir: string;
  runDir: string;
  snapshotDir: string;
};

export function repoRoot(): string {
  return resolve(import.meta.dir, "../..");
}

export function defaultContext(name: string): TestContext {
  const root = repoRoot();
  const instance = Bun.env.LNX_TEST_INSTANCE ?? `lnx-${name}-${process.pid}`;
  const base = Bun.env.LNX_BASE ?? join(Bun.env.HOME ?? ".", ".lnx");
  const imageDir = join(base, "images", instance);
  const runDir = join(base, "instances", instance);
  return {
    repoRoot: root,
    // Resolve now: tests spawn lnx from other working directories.
    lnxBin: resolve(Bun.env.LNX_BIN ?? join(root, "target/debug/lnx")),
    instance,
    base,
    tmpdir: join(tmpdir(), `lnx-${name}-${process.pid}`),
    imageDir,
    runDir,
    snapshotDir: join(imageDir, "memory-snapshots"),
  };
}

export async function cleanupContext(ctx: TestContext): Promise<void> {
  if (Bun.env.LNX_SKIP_TEST_CLEANUP === "1") {
    return;
  }
  // A detached VM owner may still be in its idle grace period; wait for it so
  // it cannot recreate directories after we remove them.
  await waitForOwnerExit(ctx).catch(() => {});
  await rm(ctx.tmpdir, { recursive: true, force: true });
  await rm(ctx.imageDir, { recursive: true, force: true });
  await rm(ctx.runDir, { recursive: true, force: true });
}

export async function waitForOwnerExit(ctx: TestContext, timeoutMs = 30_000): Promise<void> {
  const broker = join(ctx.runDir, "broker.sock");
  const lock = join(ctx.runDir, "bootstrap.lock.d");
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (!existsSync(broker) && !existsSync(lock)) {
      return;
    }
    await sleep(100);
  }
  throw new Error(`timeout waiting for VM owner exit (broker.sock or bootstrap.lock.d remains)`);
}

export async function waitForVmSuspend(ctx: TestContext, timeoutMs = 60_000): Promise<void> {
  const vmstate = join(ctx.snapshotDir, "latest", "vmstate.bin");
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (existsSync(vmstate)) {
      await waitForOwnerExit(ctx, deadline - Date.now());
      return;
    }
    await sleep(100);
  }
  throw new Error(`timeout waiting for VM suspend (missing ${vmstate})`);
}

export async function cleanupInstance(ctx: TestContext, instance: string): Promise<void> {
  await rm(join(ctx.base, "images", instance), { recursive: true, force: true });
  await rm(join(ctx.base, "instances", instance), { recursive: true, force: true });
}

export async function prepareContext(ctx: TestContext): Promise<void> {
  if (!existsSync(ctx.lnxBin)) {
    throw new Error(`missing lnx binary: ${ctx.lnxBin}`);
  }
  await cleanupContext(ctx);
  await mkdir(ctx.tmpdir, { recursive: true });
}

export async function run(
  args: string[],
  options: {
    cwd?: string;
    env?: Record<string, string | undefined>;
    stdin?: string | Uint8Array;
    timeoutMs?: number;
    check?: boolean;
  } = {},
): Promise<ExecResult> {
  const proc = Bun.spawn(args, {
    cwd: options.cwd,
    env: { ...Bun.env, ...options.env },
    stdin: options.stdin === undefined ? "ignore" : "pipe",
    stdout: "pipe",
    stderr: "pipe",
  });
  if (options.stdin !== undefined && proc.stdin) {
    await proc.stdin.write(options.stdin);
    proc.stdin.end();
  }
  let timeout: Timer | undefined;
  const timed = options.timeoutMs
    ? Promise.race([
        proc.exited,
        new Promise<number>((_, reject) => {
          timeout = setTimeout(() => {
            proc.kill("SIGKILL");
            reject(new Error(`timeout after ${options.timeoutMs}ms: ${args.join(" ")}`));
          }, options.timeoutMs);
        }),
      ])
    : proc.exited;
  const [status, stdout, stderr] = await Promise.all([
    timed.finally(() => {
      if (timeout) clearTimeout(timeout);
    }),
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
  ]);
  const result = { status, stdout: stdout.trimEnd(), stderr: stderr.trimEnd() };
  if (options.check !== false && result.status !== 0) {
    throw new Error(
      `command failed (${result.status}): ${args.join(" ")}\nstdout:\n${result.stdout}\nstderr:\n${result.stderr}`,
    );
  }
  return result;
}

export function lnx(ctx: TestContext, args: string[], options: Parameters<typeof run>[1] = {}) {
  return run([ctx.lnxBin, "--instance", ctx.instance, ...args], {
    timeoutMs: 120_000,
    ...options,
  });
}

export function spawn(
  args: string[],
  options: {
    cwd?: string;
    env?: Record<string, string | undefined>;
    stdin?: "ignore" | "pipe" | "inherit";
    stdout?: "pipe" | "inherit";
    stderr?: "pipe" | "inherit";
  } = {},
) {
  return Bun.spawn(args, {
    cwd: options.cwd,
    env: { ...Bun.env, ...options.env },
    stdin: options.stdin ?? "ignore",
    stdout: options.stdout ?? "pipe",
    stderr: options.stderr ?? "pipe",
  });
}

export function spawnLnx(
  ctx: TestContext,
  args: string[],
  options: Parameters<typeof spawn>[1] = {},
) {
  return spawn([ctx.lnxBin, "--instance", ctx.instance, ...args], options);
}

export function assertEq(got: unknown, want: unknown, label: string): void {
  if (got !== want) {
    throw new Error(`${label}: got <${String(got)}>, want <${String(want)}>`);
  }
}

export function assertContains(haystack: string, needle: string, label: string): void {
  if (!haystack.includes(needle)) {
    throw new Error(`${label}: <${haystack}> does not contain <${needle}>`);
  }
}

export function assertFile(path: string, label: string): void {
  if (!existsSync(path)) {
    throw new Error(`${label}: missing file ${path}`);
  }
}

export async function write(path: string, data: string): Promise<void> {
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, data);
}

export async function read(path: string): Promise<string> {
  return readFile(path, "utf8");
}

export async function testStep(name: string, fn: () => Promise<void>): Promise<void> {
  const start = performance.now();
  process.stderr.write(`test ${name} ... `);
  await fn();
  process.stderr.write(`ok (${Math.round(performance.now() - start)}ms)\n`);
}

export async function skippableTestStep(name: string, fn: () => Promise<void>): Promise<void> {
  const start = performance.now();
  process.stderr.write(`test ${name} ... `);
  try {
    await fn();
    process.stderr.write(`ok (${Math.round(performance.now() - start)}ms)\n`);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    process.stderr.write(`SKIP (${Math.round(performance.now() - start)}ms): ${message}\n`);
  }
}

export async function skip(reason: string): Promise<never> {
  process.stderr.write(`SKIP: ${reason}\n`);
  process.exit(0);
}

export async function commandExists(command: string): Promise<boolean> {
  const result = await run(["bash", "-lc", `command -v ${quoteShell(command)} >/dev/null`], {
    check: false,
  });
  return result.status === 0;
}

export function quoteShell(value: string): string {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

export async function sleep(ms: number): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, ms));
}

export async function fileSize(path: string): Promise<number> {
  const result = await run(["bash", "-lc", `stat -f %z ${quoteShell(path)} 2>/dev/null || stat -c %s ${quoteShell(path)}`]);
  return Number(result.stdout.trim());
}

export async function diskUsageBytes(path: string): Promise<number> {
  const result = await run(["bash", "-lc", `du -sk ${quoteShell(path)} | awk '{print $1 * 1024}'`]);
  return Number(result.stdout.trim());
}
