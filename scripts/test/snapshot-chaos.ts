// Snapshot-point chaos harness (macOS host).
//
// Each cycle starts a set of guest workloads with seeded parameters, then
// fires `lnxctl snapshot-exit` at a seeded random instant while they run, so
// capture+restore lands at arbitrary points in their execution. Every
// workload carries an exact invariant (a sequence, checksum, or file image
// that must come out byte-identical), turning silent post-restore corruption
// — dead timers, lost FUSE state, dropped stdin, RAM bit rot — into a loud
// assertion failure.
//
// Reproduce a failure with LNX_CHAOS_SEED=<seed from the log>. Scale soak
// runs with LNX_CHAOS_ITERATIONS (default 3).
import { mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import {
  assertEq,
  cleanupContext,
  defaultContext,
  lnx,
  prepareContext,
  skip,
  spawn,
  testStep,
} from "./lib";

if (process.platform !== "darwin") {
  await skip("snapshot-chaos drives the macOS/HVF snapshot path; the Linux host path is not wired up yet");
}

Bun.env.LNX_BROKER_IDLE_TTL_MS ??= "500";

const seed = Number(Bun.env.LNX_CHAOS_SEED ?? (Math.random() * 0xffff_ffff) >>> 0);
const iterations = Number(Bun.env.LNX_CHAOS_ITERATIONS ?? 3);
const ctx = defaultContext("snapshot-chaos");
const cwd = join(ctx.repoRoot, ".lnx-chaos");
const vmArgs = [
  ...(Bun.env.LNX_TEST_CPUS ? ["--cpus", Bun.env.LNX_TEST_CPUS] : []),
  ...(Bun.env.LNX_TEST_MEMORY_MIB ? ["--memory-mib", Bun.env.LNX_TEST_MEMORY_MIB] : []),
];

process.stderr.write(`snapshot-chaos seed=${seed} iterations=${iterations}\n`);

// Deterministic PRNG (mulberry32) so a seed fully reproduces the plan.
function mulberry32(a: number): () => number {
  return () => {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
const rand = mulberry32(seed);
const randInt = (lo: number, hi: number) => lo + Math.floor(rand() * (hi - lo + 1));
const randHex = (chars: number) =>
  Array.from({ length: chars }, () => "0123456789abcdef"[randInt(0, 15)]).join("");

function spawnGuest(args: string[], stdin: "ignore" | "pipe" = "ignore") {
  return spawn([ctx.lnxBin, "--instance", ctx.instance, ...vmArgs, ...args], {
    cwd,
    stdin,
    stdout: "pipe",
    stderr: "pipe",
  });
}

type Running = {
  name: string;
  proc: ReturnType<typeof spawnGuest>;
  expected: string;
  post?: () => Promise<void>;
};

async function awaitWorkload(running: Running, timeoutMs: number): Promise<void> {
  const { name, proc, expected } = running;
  let timeout: Timer | undefined;
  const timed = Promise.race([
    proc.exited,
    new Promise<number>((_, reject) => {
      timeout = setTimeout(() => {
        proc.kill("SIGKILL");
        reject(new Error(`workload ${name} timed out after ${timeoutMs}ms (seed=${seed})`));
      }, timeoutMs);
    }),
  ]);
  const [status, stdout, stderr] = await Promise.all([
    timed.finally(() => {
      if (timeout) clearTimeout(timeout);
    }),
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
  ]);
  if (status !== 0) {
    throw new Error(
      `workload ${name} exited ${status} (seed=${seed})\nstdout:\n${stdout}\nstderr:\n${stderr}`,
    );
  }
  assertEq(stdout.trimEnd(), expected, `${name} invariant (seed=${seed})`);
  if (running.post) {
    await running.post();
  }
}

// T1: timer ladder. A chain of short sleeps must keep firing across the
// restore and produce the full sequence (catches dead timer interrupts).
function timerLadder(): Running {
  const n = randInt(30, 70);
  const proc = spawnGuest([
      "bash",
    "-lc",
    `out=""; for i in $(seq 1 ${n}); do sleep 0.05; out="$out$i,"; done; echo "T1:$out"`,
  ]);
  const expected = `T1:${Array.from({ length: n }, (_, i) => i + 1).join(",")},`;
  return { name: "timer-ladder", proc, expected };
}

// T2: deterministic compute chain. vCPU registers and working memory must
// survive a restore landing mid-loop; the host computes the same LCG chain.
function computeChain(): Running {
  const start = BigInt(randInt(1, 0x7fffffff));
  const iters = randInt(500_000, 1_500_000);
  const pause = (randInt(10, 25) / 10).toFixed(1);
  const proc = spawnGuest([
    "python3",
    "-c",
    [
      "import time",
      `x = ${start}`,
      `for i in range(${iters}):`,
      "    x = (x * 6364136223846793005 + 1442695040888963407) % 18446744073709551616",
      `    if i == ${Math.floor(iters / 2)}: time.sleep(${pause})`,
      'print("T2:%x" % x)',
    ].join("\n"),
  ]);
  const m = 1n << 64n;
  let x = start;
  for (let i = 0; i < iters; i++) {
    x = (x * 6364136223846793005n + 1442695040888963407n) % m;
  }
  return { name: "compute-chain", proc, expected: `T2:${x.toString(16)}` };
}

// T3: RAM fidelity. A large seeded buffer hashed before and after the
// restore window must be identical (catches pages.img addressing bugs).
function memoryImage(): Running {
  const mib = randInt(32, 128);
  const pause = (randInt(15, 30) / 10).toFixed(1);
  const proc = spawnGuest([
    "python3",
    "-c",
    [
      "import hashlib, random, time",
      `data = random.Random(${randInt(1, 1 << 30)}).randbytes(${mib} * 1024 * 1024)`,
      "before = hashlib.sha256(data).hexdigest()",
      `time.sleep(${pause})`,
      "after = hashlib.sha256(data).hexdigest()",
      "assert before == after, (before, after)",
      'print("T3:match")',
    ].join("\n"),
  ]);
  return { name: "memory-image", proc, expected: "T3:match" };
}

// T4: open virtiofs fd. Writes before and after the restore must land in the
// same open file, and the host must see the combined image.
function virtiofsFd(cycle: number): Running {
  const a = randHex(64);
  const b = randHex(64);
  const pause = (randInt(15, 30) / 10).toFixed(1);
  const file = `chaos-fd-${cycle}.bin`;
  const proc = spawnGuest([
    "python3",
    "-c",
    [
      "import os, time",
      `f = open("${file}", "w+b")`,
      `f.write(b"${a}"); f.flush(); os.fsync(f.fileno())`,
      `time.sleep(${pause})`,
      `f.write(b"${b}"); f.flush(); os.fsync(f.fileno())`,
      "f.seek(0)",
      "data = f.read().decode()",
      `assert data == "${a}${b}", data`,
      'print("T4:match")',
    ].join("\n"),
  ]);
  return {
    name: "virtiofs-fd",
    proc,
    expected: "T4:match",
    post: async () => {
      assertEq(await readFile(join(cwd, file), "utf8"), a + b, `virtiofs-fd host file (seed=${seed})`);
    },
  };
}

// T5: stdin stream. Bytes written by the host before and after the snapshot
// must all arrive on the guest command's stdin (catches lost channel data).
function stdinStream(triggerDelayMs: number): Running {
  const chunkA = randHex(randInt(64, 256) * 1024);
  const chunkB = randHex(randInt(64, 256) * 1024);
  const proc = spawnGuest(
    [
      "python3",
      "-c",
      'import sys, hashlib; print("T5:" + hashlib.sha256(sys.stdin.buffer.read()).hexdigest())',
    ],
    "pipe",
  );
  const hasher = new Bun.CryptoHasher("sha256");
  hasher.update(chunkA);
  hasher.update(chunkB);
  const expected = `T5:${hasher.digest("hex")}`;
  (async () => {
    proc.stdin!.write(chunkA);
    proc.stdin!.flush();
    // Hold the tail until the snapshot has fired so the stream spans it.
    await new Promise((resolve) => setTimeout(resolve, triggerDelayMs + randInt(1500, 3000)));
    proc.stdin!.write(chunkB);
    proc.stdin!.end();
  })();
  return { name: "stdin-stream", proc, expected };
}

// T6: directory stream. A scandir consumed half before and half after the
// restore must yield exactly the pre-created entries (catches FUSE dirstream
// state loss).
async function dirStream(cycle: number): Promise<Running> {
  const entries = randInt(20, 50);
  const pause = (randInt(15, 30) / 10).toFixed(1);
  const dir = `chaos-dir-${cycle}`;
  await mkdir(join(cwd, dir), { recursive: true });
  const names = Array.from({ length: entries }, (_, i) => `entry-${String(i).padStart(3, "0")}`);
  for (const name of names) {
    await writeFile(join(cwd, dir, name), name);
  }
  const proc = spawnGuest([
    "python3",
    "-c",
    [
      "import os, time",
      `it = os.scandir("${dir}")`,
      `names = [next(it).name for _ in range(${Math.floor(entries / 2)})]`,
      `time.sleep(${pause})`,
      "names.extend(e.name for e in it)",
      `assert sorted(names) == ${JSON.stringify(names)}, sorted(names)`,
      'print("T6:match")',
    ].join("\n"),
  ]);
  return { name: "dir-stream", proc, expected: "T6:match" };
}

try {
  await prepareContext(ctx);
  await rm(cwd, { recursive: true, force: true });
  await mkdir(cwd, { recursive: true });

  await testStep("warm up instance", async () => {
    const ready = await lnx(ctx, [...vmArgs, "echo", "ready"], { cwd });
    assertEq(ready.stdout, "ready", "warmup boot");
  });

  for (let cycle = 1; cycle <= iterations; cycle++) {
    await testStep(`chaos cycle ${cycle}/${iterations} (seed=${seed})`, async () => {
      const triggerDelayMs = randInt(300, 1500);
      const doubleSnapshot = rand() < 0.5;
      const workloads: Running[] = [
        timerLadder(),
        computeChain(),
        memoryImage(),
        virtiofsFd(cycle),
        stdinStream(triggerDelayMs),
        await dirStream(cycle),
      ];

      await new Promise((resolve) => setTimeout(resolve, triggerDelayMs));
      const first = await lnx(ctx, [...vmArgs, "bash", "-lc", "lnxctl snapshot-exit && echo SNAP:1"], {
        cwd,
        timeoutMs: 240_000,
      });
      assertEq(first.stdout, "SNAP:1", `first snapshot trigger (seed=${seed})`);
      if (doubleSnapshot) {
        await new Promise((resolve) => setTimeout(resolve, randInt(200, 800)));
        const second = await lnx(
          ctx,
          [...vmArgs, "bash", "-lc", "lnxctl snapshot-exit && echo SNAP:2"],
          { cwd, timeoutMs: 240_000 },
        );
        assertEq(second.stdout, "SNAP:2", `second snapshot trigger (seed=${seed})`);
      }

      for (const workload of workloads) {
        await awaitWorkload(workload, 240_000);
      }
    });
  }
} finally {
  await cleanupContext(ctx);
  await rm(cwd, { recursive: true, force: true });
}
