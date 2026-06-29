import { existsSync } from "node:fs";
import { mkdir, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  cleanupContext,
  defaultContext,
  diskUsageBytes,
  fileSize,
  prepareContext,
  skip,
  testStep,
} from "./lib";

if (process.platform !== "darwin") {
  await skip("snapshot round-trip restore requires a macOS host");
}

const ctx = defaultContext("snapshot-roundtrip");
const work = join(
  ctx.repoRoot,
  "target",
  `lnx-snapshot-roundtrip-${process.pid}`,
);
const iterations = Number(Bun.env.LNX_SNAPSHOT_ROUNDTRIP_ITERATIONS ?? 2);
const maxRestoreMs = Number(Bun.env.LNX_SNAPSHOT_RESTORE_MAX_MS ?? 1500);
const largeSparseImageBytes = 8 * 1024 * 1024 * 1024;
const base = Bun.env.LNX_BASE ?? join(Bun.env.HOME ?? ".", ".lnx");
const runBase = Bun.env.LNX_RUN_BASE ?? base;
const childInstances: string[] = [];
const childWorkDirs: string[] = [];

type ChildRun = {
  pid: number;
};

function childInstance(label: string, iteration?: number): string {
  const suffix = iteration === undefined ? "" : `-${iteration}`;
  const instance = `lnx-roundtrip-${label}${suffix}-${process.pid}`;
  childInstances.push(instance);
  return instance;
}

function childTmpdir(name: string, pid: number): string {
  return join(tmpdir(), `lnx-${name}-${pid}`);
}

async function runChild(
  label: string,
  args: string[],
  env: Record<string, string | undefined>,
  timeoutMs: number,
  onStart?: (pid: number) => void,
): Promise<ChildRun> {
  const proc = Bun.spawn(args, {
    cwd: ctx.repoRoot,
    env: {
      ...Bun.env,
      LNX_BIN: ctx.lnxBin,
      LNX_SKIP_TEST_CLEANUP: "1",
      ...env,
    },
    stdout: "inherit",
    stderr: "inherit",
  });
  onStart?.(proc.pid);
  let timeout: Timer | undefined;
  const status = await Promise.race([
    proc.exited,
    new Promise<number>((_, reject) => {
      timeout = setTimeout(() => {
        proc.kill("SIGKILL");
        reject(new Error(`${label} timed out after ${timeoutMs}ms`));
      }, timeoutMs);
    }),
  ]).finally(() => {
    if (timeout) {
      clearTimeout(timeout);
    }
  });
  if (status !== 0) {
    throw new Error(`${label} exited with status ${status}`);
  }
  return { pid: proc.pid };
}

async function assertSnapshotFiles(path: string, label: string) {
  for (const file of [
    "vmstate.bin",
    "pages.img",
    "rootfs.ext4",
    "shares.stamp",
    "initramfs.stamp",
  ]) {
    const candidate = join(path, file);
    if (!existsSync(candidate)) {
      throw new Error(`${label} missing ${candidate}`);
    }
  }
  await assertSparseVmImage(join(path, "rootfs.ext4"), `${label} rootfs`);
  await assertSparseVmImage(join(path, "pages.img"), `${label} pages`);
}

async function assertSparseVmImage(path: string, label: string) {
  const size = await fileSize(path);
  const allocated = await diskUsageBytes(path);
  if (size >= largeSparseImageBytes && allocated > size / 2) {
    throw new Error(
      `${label} is not sparse enough: size=${size} allocated=${allocated}`,
    );
  }
}

async function assertRestoreUnder(path: string, label: string) {
  const timings = await readFile(path, "utf8");
  const restore = restoreElapsedMs(timings);
  if (!restore) {
    throw new Error(`${label} missing restore timing markers in ${path}`);
  }
  const elapsed = restore.elapsed;
  if (elapsed > maxRestoreMs) {
    throw new Error(
      `${label} ${restore.name} took ${elapsed.toFixed(3)}ms > ${maxRestoreMs}ms`,
    );
  }
  process.stderr.write(`${label}: ${restore.name} ${elapsed.toFixed(3)}ms\n`);
}

function restoreElapsedMs(
  timings: string,
): { name: string; elapsed: number } | undefined {
  const pairs = [
    {
      name: "snapshot restore",
      begin: "snapshot.restore.begin",
      complete: "snapshot.restore.complete",
    },
    {
      name: "microvm restore",
      begin: "build_microvm.restore_from.begin",
      complete: "build_microvm.restore_from.done",
    },
  ];
  const state = new Map<string, { begin?: number; complete?: number }>();
  for (const line of timings.split("\n")) {
    const parsed = parseTimingLine(line);
    if (!parsed) {
      continue;
    }
    const event = parsed.event.replace(/^libkrun\./, "");
    for (const pair of pairs) {
      const current = state.get(pair.name) ?? {};
      if (event === pair.begin) {
        state.set(pair.name, { begin: parsed.elapsedMs });
      } else if (event === pair.complete && current.begin !== undefined) {
        current.complete = parsed.elapsedMs;
        state.set(pair.name, current);
      }
    }
  }
  for (const pair of pairs) {
    const current = state.get(pair.name);
    if (current?.begin !== undefined && current.complete !== undefined) {
      return { name: pair.name, elapsed: current.complete - current.begin };
    }
  }
  return undefined;
}

function parseTimingLine(
  line: string,
): { elapsedMs: number; event: string } | undefined {
  const match = line.match(
    /^\s*([0-9.]+)(ms|s)\s+\+\s+[0-9.]+(?:ms|s)\s+(\S+)/,
  );
  if (!match) {
    return undefined;
  }
  return {
    elapsedMs: Number(match[1]) * (match[2] === "s" ? 1000 : 1),
    event: match[3],
  };
}

async function cleanupChildArtifacts() {
  if (Bun.env.LNX_SKIP_TEST_CLEANUP === "1") {
    return;
  }
  for (const instance of childInstances) {
    await rm(join(base, "instances", instance), {
      recursive: true,
      force: true,
    });
    await rm(join(runBase, "instances", instance), {
      recursive: true,
      force: true,
    });
  }
  for (const path of childWorkDirs) {
    await rm(path, { recursive: true, force: true });
  }
  await rm(work, { recursive: true, force: true });
}

try {
  await prepareContext(ctx);
  await rm(work, { recursive: true, force: true });
  await mkdir(work, { recursive: true });

  let linuxSnapshot = join(work, "linux-0");
  await testStep("create initial Linux/KVM snapshot fixture", async () => {
    const instance = childInstance("linux-fixture");
    const child = await runChild(
      "linux snapshot fixture",
      ["bun", "scripts/test/linux-snapshot-fixture.ts"],
      {
        LNX_TEST_INSTANCE: instance,
        LNX_LINUX_SNAPSHOT_FIXTURE_OUT: linuxSnapshot,
      },
      1_200_000,
      (pid) => {
        childWorkDirs.push(
          join(ctx.repoRoot, "target", `lnx-linux-fixture-${pid}`),
        );
        childWorkDirs.push(childTmpdir("linux-snapshot-fixture", pid));
      },
    );
    await assertSnapshotFiles(linuxSnapshot, "initial Linux snapshot");
  });

  for (let iteration = 1; iteration <= iterations; iteration += 1) {
    const macosSnapshot = join(work, `macos-${iteration}`);
    await testStep(
      `round-trip ${iteration}/${iterations}: Linux/KVM snapshot restores on macOS/HVF`,
      async () => {
        const instance = childInstance("linux-macos", iteration);
        const child = await runChild(
          `Linux-to-macOS restore ${iteration}`,
          ["bun", "scripts/test/linux-macos-snapshot.ts"],
          {
            LNX_TEST_INSTANCE: instance,
            LNX_LINUX_SNAPSHOT_FIXTURE: linuxSnapshot,
            LNX_LINUX_MACOS_EXPORT_MACOS_SNAPSHOT: macosSnapshot,
            LNX_LINUX_SNAPSHOT_EXPECTED_DISK: "linux-disk",
            LNX_LINUX_SNAPSHOT_EXPECTED_MEMORY: "linux-memory",
            LNX_LINUX_SNAPSHOT_EXPECTED_AFTER: "linux-after",
          },
          420_000,
          (pid) => {
            childWorkDirs.push(childTmpdir("linux-macos-snapshot", pid));
          },
        );
        await assertSnapshotFiles(macosSnapshot, `macOS snapshot ${iteration}`);
        await assertRestoreUnder(
          join(runBase, "instances", instance, "timings.log"),
          `Linux-to-macOS restore ${iteration}`,
        );
      },
    );

    const nextLinuxSnapshot = join(work, `linux-${iteration}`);
    await testStep(
      `round-trip ${iteration}/${iterations}: macOS/HVF snapshot restores on Linux/KVM`,
      async () => {
        const instance = childInstance("macos-linux", iteration);
        const nestedWorkDir = join(
          base,
          "test-work",
          `snapshot-roundtrip-nested-${iteration}-${process.pid}`,
        );
        childInstances.push(`${instance}-macos-linux-cold`);
        childInstances.push(`${instance}-macos-linux`);
        const child = await runChild(
          `macOS-to-Linux restore ${iteration}`,
          ["bun", "scripts/test/nested-kvm.ts"],
          {
            LNX_TEST_INSTANCE: instance,
            LNX_PRESERVE_NESTED_KVM: "1",
            LNX_NESTED_ONLY_MACOS_LINUX: "1",
            LNX_MACOS_SNAPSHOT_FIXTURE: macosSnapshot,
            LNX_MACOS_SNAPSHOT_EXPECTED_DISK: "linux-disk",
            LNX_MACOS_SNAPSHOT_EXPECTED_MEMORY: "linux-memory",
            LNX_MACOS_SNAPSHOT_EXPECTED_AFTER: "linux-after",
            LNX_NESTED_MACOS_LINUX_EXPORT_LINUX_SNAPSHOT: nextLinuxSnapshot,
            LNX_NESTED_MACOS_LINUX_INNER_TIMEOUT_MS: "160000",
            LNX_NESTED_KVM_WORKDIR: nestedWorkDir,
          },
          600_000,
          (pid) => {
            childWorkDirs.push(nestedWorkDir);
            childWorkDirs.push(childTmpdir("nested-kvm", pid));
          },
        );
        await assertSnapshotFiles(
          nextLinuxSnapshot,
          `Linux snapshot ${iteration}`,
        );
        await assertRestoreUnder(
          join(
            nestedWorkDir,
            "macos-linux",
            "instances",
            `mli-${child.pid}`,
            "timings.log",
          ),
          `macOS-to-Linux restore ${iteration}`,
        );
        linuxSnapshot = nextLinuxSnapshot;
      },
    );
  }
} finally {
  await cleanupChildArtifacts();
  await cleanupContext(ctx);
}
