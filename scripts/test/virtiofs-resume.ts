import {
  mkdir,
  rm } from "node:fs/promises";
import { join } from "node:path";
import {
  assertContains,
  assertEq,
  cleanupContext,
  cleanupInstance,
  defaultContext,
  prepareContext,
  read,
  run,
  sleep,
  testStep,
  waitForVmSuspend,
  write,
} from "./lib";

const ctx = defaultContext("virtiofs-resume");
const forkInstance = `${ctx.instance}-fork`;
const cwd = join(ctx.repoRoot, ".lnx-virtiofs-resume");
const outsideCwd = `/tmp/lnx-virtiofs-resume-outside-${process.pid}`;

async function cleanupDirs() {
  await rm(cwd, { recursive: true, force: true });
  await rm(outsideCwd, { recursive: true, force: true });
}

async function discardLatestSnapshot() {
  await rm(join(ctx.snapshotDir, "latest"), { recursive: true, force: true });
}

async function waitForSourceReady(): Promise<void> {
  const deadline = Date.now() + 120_000;
  while (Date.now() < deadline) {
    const result = await ctx.vm.cli(["bash", "-lc", "test -e /tmp/virtiofs-fork-ready"], {
      check: false,
      cwd,
    });
    if (result.status === 0) return;
    await sleep(250);
  }
  throw new Error("timed out waiting for source VM fork-ready marker");
}

try {
  await prepareContext(ctx);
  await cleanupInstance(ctx, forkInstance);
  await cleanupDirs();
  await mkdir(cwd, { recursive: true });
  await mkdir(outsideCwd, { recursive: true });

  await testStep("open virtiofs fd and mmap survive snapshot-exit", async () => {
    const result = await ctx.vm.cli([
        "python3",
        "-",
      ],
      {
        cwd,
        timeoutMs: 180_000,
        stdin: String.raw`
import mmap
import os
import subprocess

mount = subprocess.check_output(["findmnt", "-T", os.getcwd(), "-no", "FSTYPE,OPTIONS"], text=True).strip()
assert mount.startswith("virtiofs ") and "dax=always" in mount, mount

fd = os.open("snapshot-exit-open.bin", os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o644)
try:
    os.ftruncate(fd, 4096)
    os.write(fd, b"fd-before\n")
    view = mmap.mmap(fd, 4096, access=mmap.ACCESS_WRITE)
    try:
        mmap_before = b"mmap-before"
        mmap_after = b"mmap-after"
        view[128:128 + len(mmap_before)] = mmap_before
        view.flush()
        os.fsync(fd)
        subprocess.run(["lnxctl", "snapshot-exit"], check=True)
        os.lseek(fd, 16, os.SEEK_SET)
        os.write(fd, b"fd-after\n")
        view[256:256 + len(mmap_after)] = mmap_after
        view.flush()
        os.fsync(fd)
        os.lseek(fd, 0, os.SEEK_SET)
        data = os.read(fd, 512)
        assert b"fd-before" in data, data
        assert b"fd-after" in data, data
        assert data[128:128 + len(mmap_before)] == mmap_before, data[128:140]
        assert data[256:256 + len(mmap_after)] == mmap_after, data[256:267]
        print("snapshot-exit-open-ok")
    finally:
        view.close()
finally:
    os.close(fd)
`,
      },
    );
    assertContains(result.stdout, "snapshot-exit-open-ok", "open fd and mmap survived snapshot-exit");
    const host = await read(join(cwd, "snapshot-exit-open.bin"));
    assertContains(host, "fd-before", "host saw pre-snapshot fd write");
    assertContains(host, "fd-after", "host saw post-snapshot fd write");
  });

  await testStep("restored virtiofs sees host edits after prior guest read", async () => {
    await waitForVmSuspend(ctx);
    await discardLatestSnapshot();
    await write(join(cwd, "host-edited.txt"), "before\n");

    const before = await ctx.vm.cli(["cat", "host-edited.txt"], { cwd, timeoutMs: 180_000 });
    assertEq(before.stdout, "before", "guest read initial host file");
    await waitForVmSuspend(ctx);

    await write(join(cwd, "host-edited.txt"), "after-after-after\n");

    const after = await ctx.vm.cli(["cat", "host-edited.txt"], { cwd, timeoutMs: 180_000 });
    assertEq(after.stdout, "after-after-after", "restored guest read host-edited file");
  });

  await testStep("restored outside-home virtiofs sees host edits after prior guest read", async () => {
    await waitForVmSuspend(ctx);
    await discardLatestSnapshot();
    await write(join(outsideCwd, "outside-host-edited.txt"), "outside-before\n");

    const before = await ctx.vm.cli(["cat", "outside-host-edited.txt"], {
      cwd: outsideCwd,
      timeoutMs: 180_000,
    });
    assertEq(before.stdout, "outside-before", "outside-home guest read initial host file");
    await waitForVmSuspend(ctx);

    await write(join(outsideCwd, "outside-host-edited.txt"), "outside-after-after\n");

    const after = await ctx.vm.cli(["cat", "outside-host-edited.txt"], {
      cwd: outsideCwd,
      timeoutMs: 180_000,
    });
    assertEq(after.stdout, "outside-after-after", "restored outside-home guest read host-edited file");
  });

  await testStep("open virtiofs fd and mmap survive fork restore", async () => {
    await waitForVmSuspend(ctx);
    await discardLatestSnapshot();

    const owner = ctx.vm.spawnCli([
        "python3",
        "-",
      ],
      { cwd, stdin: "pipe" },
    );
    const ownerScript = String.raw`
import mmap
import os
import pathlib
import subprocess
import sys
import time

mount = subprocess.check_output(["findmnt", "-T", os.getcwd(), "-no", "FSTYPE,OPTIONS"], text=True).strip()
assert mount.startswith("virtiofs ") and "dax=always" in mount, mount

for marker in [
    "/tmp/virtiofs-fork-ready",
    "/tmp/virtiofs-fork-go",
    "/tmp/virtiofs-fork-result",
    "/tmp/virtiofs-fork-result.tmp",
]:
    try:
        pathlib.Path(marker).unlink()
    except FileNotFoundError:
        pass

fd = os.open("fork-open.bin", os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o644)
os.ftruncate(fd, 4096)
view = mmap.mmap(fd, 4096, access=mmap.ACCESS_WRITE)
try:
    os.write(fd, b"source-fd-before\n")
    source_mmap_before = b"source-mmap-before"
    fork_mmap_after = b"fork-mmap-after"
    view[128:128 + len(source_mmap_before)] = source_mmap_before
    view.flush()
    os.fsync(fd)
    pathlib.Path("/tmp/virtiofs-fork-ready").write_text("ready")
    deadline = time.time() + 180
    while not pathlib.Path("/tmp/virtiofs-fork-go").exists():
        if time.time() > deadline:
            raise SystemExit("timed out waiting for fork-go marker")
        time.sleep(0.1)
    os.lseek(fd, 32, os.SEEK_SET)
    os.write(fd, b"fork-fd-after\n")
    view[256:256 + len(fork_mmap_after)] = fork_mmap_after
    view.flush()
    os.fsync(fd)
    os.lseek(fd, 0, os.SEEK_SET)
    data = os.read(fd, 512)
    checks = [
        b"source-fd-before" in data,
        data[128:128 + len(source_mmap_before)] == source_mmap_before,
        b"fork-fd-after" in data,
        data[256:256 + len(fork_mmap_after)] == fork_mmap_after,
    ]
    result = pathlib.Path("/tmp/virtiofs-fork-result")
    tmp_result = pathlib.Path("/tmp/virtiofs-fork-result.tmp")
    tmp_result.write_text("ok" if all(checks) else repr(data))
    os.replace(tmp_result, result)
    print("fork-worker-done", flush=True)
finally:
    view.close()
    os.close(fd)
`;
    await owner.stdin?.write(ownerScript);
    owner.stdin?.end();

    try {
      await waitForSourceReady();
      assertEq(
        (await run([ctx.lnxBin, "--instance", ctx.instance, "fork", forkInstance], {
          timeoutMs: 240_000,
        })).stdout,
        forkInstance,
        "fork name",
      );
      const forked = await run(
        [
          ctx.lnxBin,
          "--instance",
          forkInstance,
          "bash",
          "-lc",
          "touch /tmp/virtiofs-fork-go; for i in $(seq 1 600); do test -s /tmp/virtiofs-fork-result && break; sleep 0.1; done; test -s /tmp/virtiofs-fork-result; cat /tmp/virtiofs-fork-result",
        ],
        { cwd, timeoutMs: 180_000 },
      );
      assertEq(forked.stdout, "ok", "fork restored process kept fd and mmap usable");
      const host = await read(join(cwd, "fork-open.bin"));
      assertContains(host, "source-fd-before", "host saw source fd write");
      assertContains(host, "fork-fd-after", "host saw fork fd write");
    } finally {
      owner.kill("SIGTERM");
      await owner.exited.catch(() => {});
    }
  });
} finally {
  await cleanupContext(ctx);
  await cleanupInstance(ctx, forkInstance);
  await cleanupDirs();
}
