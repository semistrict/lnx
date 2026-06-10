import { existsSync } from "node:fs";
import { mkdir, rm, writeFile, copyFile } from "node:fs/promises";
import { join, resolve } from "node:path";

type Stats = {
  median: number;
  p95: number;
  p99: number;
};

type VmStartIteration = {
  instance: string;
  ttiMs: number;
  forkMs?: number;
  stdout?: string;
  error?: string;
};

type VmStartResult = {
  provider: "lnx";
  mode: "burst" | "sequential";
  command: string[];
  concurrency: number;
  cpus: number;
  memoryMiB: number;
  iterations: VmStartIteration[];
  summary: {
    ttiMs: Stats;
  };
  wallClockMs: number;
  timeToFirstReadyMs: number;
  successRate: number;
};

type ExecResult = {
  status: number;
  stdout: string;
  stderr: string;
};

const args = process.argv.slice(2);
const lnxBin = getArgValue("--lnx-bin") ?? process.env.LNX_BIN ?? resolve("target/debug/lnx");
const base = getArgValue("--base") ?? process.env.LNX_BASE ?? join(process.env.HOME ?? ".", ".lnx");
const mode = parseMode(getArgValue("--mode") ?? "burst");
const iterations = Number(getArgValue("--iterations") ?? "100");
const concurrencyList = parseConcurrencyList(getArgValue("--concurrency") ?? getArgValue("--concurrency-list") ?? "10,20,50,100");
const cpus = Number(getArgValue("--cpus") ?? "1");
const memoryMiB = Number(getArgValue("--memory-mib") ?? "512");
const timeoutMs = Number(getArgValue("--timeout-ms") ?? "300000");
const prefix = getArgValue("--prefix") ?? `lnx-vm-start-${process.pid}`;
const sourceInstance = getArgValue("--source-instance") ?? process.env.LNX_BENCH_SOURCE_INSTANCE ?? "default";
const sourceCheckpoint = getArgValue("--source-checkpoint") ?? `${prefix}-source`;
const keep = hasFlag("--keep");
const keepFailed = hasFlag("--keep-failed");
const skipInit = hasFlag("--skip-init");
const skipPreflight = hasFlag("--skip-preflight");
const dryRun = hasFlag("--dry-run");
const resultsDir = resolve(getArgValue("--results-dir") ?? "results/vm-start");
const command = ["python3", "-c", "print('hello world')"];

function getArgValue(flag: string): string | undefined {
  const index = args.indexOf(flag);
  return index >= 0 && index + 1 < args.length ? args[index + 1] : undefined;
}

function hasFlag(flag: string): boolean {
  return args.includes(flag);
}

function parseConcurrencyList(value: string): number[] {
  const parsed = value
    .split(",")
    .map((part) => Number(part.trim()))
    .filter((value) => Number.isInteger(value) && value > 0);
  if (parsed.length === 0) {
    throw new Error(`invalid --concurrency list: ${value}`);
  }
  return parsed;
}

function parseMode(value: string): "burst" | "sequential" {
  if (value === "burst" || value === "sequential") {
    return value;
  }
  throw new Error(`invalid --mode ${value}; expected burst or sequential`);
}

async function run(
  argv: string[],
  options: {
    timeoutMs?: number;
    env?: Record<string, string | undefined>;
    check?: boolean;
  } = {},
): Promise<ExecResult> {
  const proc = Bun.spawn(argv, {
    env: { ...process.env, ...options.env },
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
  });

  let timeout: Timer | undefined;
  const exited = options.timeoutMs
    ? Promise.race([
        proc.exited,
        new Promise<number>((_, reject) => {
          timeout = setTimeout(() => {
            proc.kill("SIGKILL");
            reject(new Error(`timeout after ${options.timeoutMs}ms: ${argv.join(" ")}`));
          }, options.timeoutMs);
        }),
      ])
    : proc.exited;

  const [status, stdout, stderr] = await Promise.all([
    exited.finally(() => {
      if (timeout) clearTimeout(timeout);
    }),
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
  ]);
  const result = { status, stdout: stdout.trimEnd(), stderr: stderr.trimEnd() };
  if (options.check !== false && status !== 0) {
    throw new Error(`command failed (${status}): ${argv.join(" ")}\nstdout:\n${result.stdout}\nstderr:\n${result.stderr}`);
  }
  return result;
}

async function removeInstance(instance: string): Promise<void> {
  await rm(join(base, "images", instance), { recursive: true, force: true });
  await rm(join(base, "instances", instance), { recursive: true, force: true });
}

async function benchmarkOne(instance: string): Promise<VmStartIteration> {
  const start = performance.now();
  try {
    const forkStart = performance.now();
    await run(
      [
        lnxBin,
        "--instance",
        sourceInstance,
        "--cpus",
        String(cpus),
        "--memory-mib",
        String(memoryMiB),
        "fork",
        "--checkpoint",
        sourceCheckpoint,
        instance,
      ],
      { timeoutMs, env: { LNX_BASE: base } },
    );
    const forkMs = performance.now() - forkStart;
    const result = await run(
      [
        lnxBin,
        "--instance",
        instance,
        "--cpus",
        String(cpus),
        "--memory-mib",
        String(memoryMiB),
        ...command,
      ],
      { timeoutMs, env: { LNX_BASE: base } },
    );
    const ttiMs = performance.now() - start;
    if (result.stdout !== "hello world") {
      return {
        instance,
        ttiMs,
        forkMs,
        stdout: result.stdout,
        error: `unexpected stdout: ${JSON.stringify(result.stdout)}`,
      };
    }
    return { instance, ttiMs, forkMs, stdout: result.stdout };
  } catch (error) {
    return {
      instance,
      ttiMs: performance.now() - start,
      error: error instanceof Error ? error.message : String(error),
    };
  }
}

async function preflight(): Promise<void> {
  const instance = `${prefix}-preflight`;
  await removeInstance(instance);
  try {
    await run(
      [
        lnxBin,
        "--instance",
        sourceInstance,
        "--cpus",
        String(cpus),
        "--memory-mib",
        String(memoryMiB),
        "fork",
        "--checkpoint",
        sourceCheckpoint,
        instance,
      ],
      { timeoutMs, env: { LNX_BASE: base } },
    );
    const result = await run(
      [
        lnxBin,
        "--instance",
        instance,
        "--cpus",
        String(cpus),
        "--memory-mib",
        String(memoryMiB),
        ...command,
      ],
      { timeoutMs, env: { LNX_BASE: base } },
    );
    if (result.stdout !== "hello world") {
      throw new Error(`unexpected preflight stdout: ${JSON.stringify(result.stdout)}`);
    }
  } catch (error) {
    throw new Error(
      [
        "benchmark preflight failed before fanout",
        "The rootfs must be able to run: python3 -c \"print('hello world')\".",
        "Rebuild or refresh the default lnx rootfs if this image predates python3-minimal.",
        error instanceof Error ? error.message : String(error),
      ].join("\n"),
    );
  } finally {
    await removeInstance(instance);
  }
}

async function prepareSourceCheckpoint(): Promise<void> {
  const result = await run(
    [
      lnxBin,
      "--instance",
      sourceInstance,
      "--cpus",
      String(cpus),
      "--memory-mib",
      String(memoryMiB),
      "checkpoint",
      "-m",
      sourceCheckpoint,
    ],
    { timeoutMs, env: { LNX_BASE: base } },
  );
  if (result.stdout !== sourceCheckpoint) {
    throw new Error(`unexpected source checkpoint output: ${JSON.stringify(result.stdout)}`);
  }
}

async function runBurst(concurrency: number): Promise<VmStartResult> {
  console.log(`\n--- Burst VM start benchmark: lnx (${concurrency} VMs) ---`);
  const instances = Array.from({ length: concurrency }, (_, index) => `${prefix}-${concurrency}-${index}`);
  await Promise.all(instances.map(removeInstance));

  const wallStart = performance.now();
  const iterations = await Promise.all(
    instances.map(async (instance, index) => {
      const result = await benchmarkOne(instance);
      if (result.error) {
        console.log(`  VM ${index + 1}/${concurrency}: FAILED - ${result.error.split("\n")[0]}`);
      } else {
        console.log(`  VM ${index + 1}/${concurrency}: TTI ${formatSeconds(result.ttiMs)}s`);
      }
      return result;
    }),
  );
  const wallClockMs = performance.now() - wallStart;
  const successful = iterations.filter((iteration) => !iteration.error);
  const successfulTimes = successful.map((iteration) => iteration.ttiMs);
  const timeToFirstReadyMs = successfulTimes.length > 0 ? Math.min(...successfulTimes) : 0;

  console.log(
    `  Wall clock: ${formatSeconds(wallClockMs)}s | First ready: ${formatSeconds(timeToFirstReadyMs)}s | Success: ${successful.length}/${concurrency}`,
  );

  if (!keep) {
    await Promise.all(
      iterations
        .filter((iteration) => !keepFailed || !iteration.error)
        .map((iteration) => removeInstance(iteration.instance)),
    );
  }

  return {
    provider: "lnx",
    mode: "burst",
    command,
    concurrency,
    cpus,
    memoryMiB,
    iterations,
    summary: {
      ttiMs: successfulTimes.length > 0 ? computeStats(successfulTimes) : { median: 0, p95: 0, p99: 0 },
    },
    wallClockMs,
    timeToFirstReadyMs,
    successRate: concurrency === 0 ? 0 : successful.length / concurrency,
  };
}

async function runSequential(iterationCount: number): Promise<VmStartResult> {
  console.log(`\n--- Sequential VM start benchmark: lnx (${iterationCount} iterations) ---`);
  const wallStart = performance.now();
  const results: VmStartIteration[] = [];
  let timeToFirstReadyMs = 0;

  for (let index = 0; index < iterationCount; index++) {
    const instance = `${prefix}-sequential-${index}`;
    await removeInstance(instance);
    const result = await benchmarkOne(instance);
    if (!result.error && timeToFirstReadyMs === 0) {
      timeToFirstReadyMs = performance.now() - wallStart;
    }
    if (result.error) {
      console.log(`  VM ${index + 1}/${iterationCount}: FAILED - ${result.error.split("\n")[0]}`);
    } else {
      console.log(`  VM ${index + 1}/${iterationCount}: TTI ${formatSeconds(result.ttiMs)}s`);
    }
    results.push(result);
    if (!keep && (!keepFailed || !result.error)) {
      await removeInstance(instance);
    }
  }

  const wallClockMs = performance.now() - wallStart;
  const successful = results.filter((iteration) => !iteration.error);
  const successfulTimes = successful.map((iteration) => iteration.ttiMs);
  console.log(
    `  Wall clock: ${formatSeconds(wallClockMs)}s | First ready: ${formatSeconds(timeToFirstReadyMs)}s | Success: ${successful.length}/${iterationCount}`,
  );

  return {
    provider: "lnx",
    mode: "sequential",
    command,
    concurrency: 1,
    cpus,
    memoryMiB,
    iterations: results,
    summary: {
      ttiMs: successfulTimes.length > 0 ? computeStats(successfulTimes) : { median: 0, p95: 0, p99: 0 },
    },
    wallClockMs,
    timeToFirstReadyMs,
    successRate: iterationCount === 0 ? 0 : successful.length / iterationCount,
  };
}

function percentile(sorted: number[], p: number): number {
  if (sorted.length === 0) return 0;
  const index = Math.max(0, Math.ceil((p / 100) * sorted.length) - 1);
  return sorted[Math.min(index, sorted.length - 1)];
}

function computeStats(values: number[], trimPercent = 0.05): Stats {
  if (values.length === 0) return { median: 0, p95: 0, p99: 0 };
  const sorted = [...values].sort((a, b) => a - b);
  const trimCount = Math.floor(sorted.length * trimPercent);
  const trimmed = trimCount > 0 && sorted.length - 2 * trimCount > 0 ? sorted.slice(trimCount, sorted.length - trimCount) : sorted;
  const mid = Math.floor(trimmed.length / 2);
  const median = trimmed.length % 2 === 0 ? (trimmed[mid - 1] + trimmed[mid]) / 2 : trimmed[mid];
  return {
    median,
    p95: percentile(trimmed, 95),
    p99: percentile(trimmed, 99),
  };
}

function formatSeconds(ms: number): string {
  return (ms / 1000).toFixed(2);
}

function round(value: number): number {
  return Math.round(value * 100) / 100;
}

function cleanResult(result: VmStartResult): VmStartResult {
  return {
    ...result,
    wallClockMs: round(result.wallClockMs),
    timeToFirstReadyMs: round(result.timeToFirstReadyMs),
    successRate: round(result.successRate),
    iterations: result.iterations.map((iteration) => ({
      ...iteration,
      ttiMs: round(iteration.ttiMs),
    })),
    summary: {
      ttiMs: {
        median: round(result.summary.ttiMs.median),
        p95: round(result.summary.ttiMs.p95),
        p99: round(result.summary.ttiMs.p99),
      },
    },
  };
}

function printTable(results: VmStartResult[]): void {
  const header = ["Mode", "Count", "Median (s)", "P95 (s)", "P99 (s)", "Wall (s)", "First (s)", "Status"].join(" | ");
  const separator = ["----------", "-----", "----------", "-------", "-------", "--------", "---------", "------"].join("-+-");
  console.log("\n" + "=".repeat(separator.length));
  console.log(`  LNX VM START BENCHMARK - ${mode.toUpperCase()} TTI`);
  console.log("=".repeat(separator.length));
  console.log(header);
  console.log(separator);
  for (const result of results) {
    const successful = result.iterations.filter((iteration) => !iteration.error).length;
    console.log(
      [
        result.mode.padEnd(10),
        String(result.iterations.length).padEnd(5),
        formatSeconds(result.summary.ttiMs.median).padEnd(10),
        formatSeconds(result.summary.ttiMs.p95).padEnd(7),
        formatSeconds(result.summary.ttiMs.p99).padEnd(7),
        formatSeconds(result.wallClockMs).padEnd(8),
        formatSeconds(result.timeToFirstReadyMs).padEnd(9),
        `${successful}/${result.iterations.length} OK`,
      ].join(" | "),
    );
  }
  console.log("=".repeat(separator.length));
  console.log("  TTI = fork existing VM, then lnx invocation to successful guest Python hello-world output.\n");
}

async function writeResults(results: VmStartResult[]): Promise<void> {
  await mkdir(resultsDir, { recursive: true });
  const timestamp = new Date().toISOString().replaceAll(":", "-");
  const output = {
    version: "1.0",
    timestamp: new Date().toISOString(),
    environment: {
      bun: Bun.version,
      platform: process.platform,
      arch: process.arch,
    },
    config: {
      lnxBin,
      base,
      mode,
      sourceInstance,
      sourceCheckpoint,
      command,
      iterations,
      concurrency: concurrencyList,
      cpus,
      memoryMiB,
      timeoutMs,
      keep,
      keepFailed,
      skipPreflight,
    },
    results: results.map(cleanResult),
  };
  const outPath = join(resultsDir, `${timestamp}.json`);
  await writeFile(outPath, JSON.stringify(output, null, 2));
  await copyFile(outPath, join(resultsDir, "latest.json"));
  console.log(`Results written to ${outPath}`);
  console.log(`Copied latest: ${join(resultsDir, "latest.json")}`);
}

async function main(): Promise<void> {
  if (hasFlag("--help")) {
    console.log(`Usage: bun run bench:vm-start -- [options]

Options:
  --mode burst|sequential       Benchmark mode
  --iterations 100              Sequential iterations
  --concurrency 10,20,50,100   Comma-separated burst sizes
  --lnx-bin target/debug/lnx    lnx binary to benchmark
  --base ~/.lnx                 LNX_BASE state directory
  --source-instance default     Existing VM to fork for each benchmark VM
  --source-checkpoint NAME      Existing or created source checkpoint name
  --cpus 1                      vCPUs per VM
  --memory-mib 512              Memory per VM
  --timeout-ms 300000           Timeout per VM
  --prefix lnx-vm-start-PID     Instance name prefix
  --results-dir results/vm-start
  --keep                        Keep benchmark instances after the run
  --keep-failed                 Keep only failed instances after the run
  --skip-init                   Do not run global lnx init before timing
  --skip-preflight              Do not run one Python smoke VM before fanout
  --dry-run                     Print planned commands without running
`);
    return;
  }

  if (!existsSync(lnxBin)) {
    throw new Error(`missing lnx binary: ${lnxBin}; run bun run build first or pass --lnx-bin`);
  }

  console.log("lnx VM start benchmark");
  console.log(`Date: ${new Date().toISOString()}`);
  console.log(`Mode: ${mode}`);
  if (mode === "sequential") {
    console.log(`Iterations: ${iterations}`);
  } else {
    console.log(`Concurrency: ${concurrencyList.join(", ")}`);
  }
  console.log(`Command: ${command.join(" ")}`);
  console.log(`State: ${base}`);
  console.log(`Source VM: ${sourceInstance}`);
  console.log(`Source checkpoint: ${sourceCheckpoint}`);
  console.log(`Per VM: ${cpus} CPU, ${memoryMiB} MiB RAM\n`);

  if (dryRun) {
    if (mode === "sequential") {
      console.log(`Would sequentially fork ${iterations} instances from ${sourceInstance} named ${prefix}-sequential-<n>`);
    } else {
      for (const concurrency of concurrencyList) {
        console.log(`Would fork ${concurrency} instances from ${sourceInstance} named ${prefix}-${concurrency}-<n>`);
      }
    }
    return;
  }

  if (!skipInit) {
    console.log("--- Global image init, outside timed benchmark ---");
    await run([lnxBin, "init"], { timeoutMs, env: { LNX_BASE: base } });
  }

  console.log("--- Source checkpoint, outside timed benchmark ---");
  await prepareSourceCheckpoint();

  if (!skipPreflight) {
    console.log("--- Python command preflight, outside timed benchmark ---");
    await preflight();
  }

  const results: VmStartResult[] = [];
  if (mode === "sequential") {
    results.push(await runSequential(iterations));
  } else {
    for (const concurrency of concurrencyList) {
      results.push(await runBurst(concurrency));
    }
  }
  printTable(results);
  await writeResults(results);
  const failures = results.flatMap((result) => result.iterations.filter((iteration) => iteration.error));
  if (failures.length > 0) {
    throw new Error(`benchmark failed: ${failures.length} VM iteration(s) failed`);
  }
}

try {
  await main();
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
}
