import { mkdir, readdir, readFile, rm } from "node:fs/promises";
import { join } from "node:path";
import {
  assertContains,
  assertEq,
  cleanupContext,
  cleanupInstance,
  defaultContext,
  lnx,
  prepareContext,
  read,
  run,
  testStep,
} from "./lib";

const ctx = defaultContext("virtiofs-policy");
const forkInstance = `${ctx.instance}-fork`;
const cwdA = join(ctx.repoRoot, ".lnx-virtiofs-policy-a");
const cwdB = join(ctx.repoRoot, ".lnx-virtiofs-policy-b");

async function cleanupDirs() {
  await rm(cwdA, { recursive: true, force: true });
  await rm(cwdB, { recursive: true, force: true });
}

async function checkpointPathByName(name: string): Promise<string> {
  const checkpointDir = join(ctx.imageDir, "checkpoints");
  for (const entry of await readdir(checkpointDir, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    const path = join(checkpointDir, entry.name);
    const meta = await readFile(join(path, "checkpoint.meta"), "utf8");
    if (meta.split("\n").includes(`name=${name}`)) return path;
  }
  throw new Error(`checkpoint not found: ${name}`);
}

try {
  await prepareContext(ctx);
  await cleanupInstance(ctx, forkInstance);
  await cleanupDirs();
  await mkdir(cwdA, { recursive: true });
  await mkdir(cwdB, { recursive: true });

  await testStep("home virtiofs allows current cwd writes and denies sibling writes", async () => {
    const result = await lnx(
      ctx,
      [
        "--no-snapshot-restore",
        "bash",
        "-lc",
        [
          "printf allowed > allowed.txt",
          `if printf denied > ${cwdB}/denied.txt 2>/tmp/deny.err; then echo deny-unexpected; else echo deny-ok; fi`,
          "cat allowed.txt",
        ].join("; "),
      ],
      { cwd: cwdA },
    );
    assertContains(result.stdout, "deny-ok", "sibling write denied");
    assertContains(result.stdout, "allowed", "cwd write allowed");
    assertEq(await read(join(cwdA, "allowed.txt")), "allowed", "host saw cwd write");
    const denied = await run(["test", "!", "-e", join(cwdB, "denied.txt")], { check: false });
    assertEq(denied.status, 0, "denied file absent on host");
  });

  await testStep("snapshot restore updates writable cwd allowlist", async () => {
    const checkpointName = "virtiofs-policy-memory";
    await lnx(
      ctx,
      [
        "bash",
        "-lc",
        "printf memory-policy | sudo tee /run/virtiofs-policy-memory >/dev/null; printf snapshot-root | sudo tee /root/virtiofs-policy-root >/dev/null; printf before > before-snapshot.txt",
      ],
      { cwd: cwdA },
    );
    assertEq(
      (await run([ctx.lnxBin, "--instance", ctx.instance, "checkpoint", "-m", checkpointName])).stdout,
      checkpointName,
      "checkpoint name",
    );
    const snapshot = await checkpointPathByName(checkpointName);
    const restored = await lnx(
      ctx,
      [
        "--snapshot",
        snapshot,
        "bash",
        "-lc",
        [
          'sudo cat /root/virtiofs-policy-root',
          "printf after > after-restore.txt",
          `if printf denied > ${cwdA}/after-denied.txt 2>/tmp/deny.err; then echo deny-unexpected; else echo deny-ok; fi`,
        ].join("; "),
      ],
      { cwd: cwdB },
    );
    assertContains(restored.stdout, "snapshot-root", "explicit snapshot rootfs restored");
    assertContains(restored.stdout, "deny-ok", "old cwd write denied after restore");
    assertEq(await read(join(cwdB, "after-restore.txt")), "after", "new cwd write allowed after restore");
  });

  await testStep("copy_file_range and hole punching work through APFS virtiofs", async () => {
    const result = await lnx(
      ctx,
      [
        "bash",
        "-lc",
        String.raw`
set -euo pipefail
python3 - <<'PY'
import os
with open("sparse-src.bin", "wb") as f:
    f.write(b"head")
    f.seek(2 * 1024 * 1024)
    f.write(b"tail")
src = os.open("sparse-src.bin", os.O_RDONLY)
dst = os.open("sparse-dst.bin", os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o644)
try:
    copied = os.copy_file_range(src, dst, 2 * 1024 * 1024 + 4)
finally:
    os.close(src)
    os.close(dst)
print(f"copied={copied}")
with open("sparse-dst.bin", "rb") as f:
    assert f.read(4) == b"head"
    f.seek(2 * 1024 * 1024)
    assert f.read(4) == b"tail"
with open("punch.bin", "wb") as f:
    f.write(b"A" * (3 * 1024 * 1024))
PY
fallocate --punch-hole --keep-size --offset 1048576 --length 1048576 punch.bin
python3 - <<'PY'
with open("punch.bin", "rb") as f:
    f.seek(1048576)
    assert f.read(4096) == b"\0" * 4096
print("hole-ok")
PY
`,
      ],
      { cwd: cwdA },
    );
    assertContains(result.stdout, "copied=2097156", "copy_file_range copied sparse range");
    assertContains(result.stdout, "hole-ok", "punched range reads as zeros");
    assertEq(
      (await run(["cmp", "-s", join(cwdA, "sparse-src.bin"), join(cwdA, "sparse-dst.bin")], { check: false }))
        .status,
      0,
      "copied bytes match",
    );
  });

  await testStep("fork preserves policy-backed home virtiofs", async () => {
    const fork = await run([ctx.lnxBin, "--instance", ctx.instance, "fork", forkInstance]);
    assertEq(fork.stdout, forkInstance, "fork name");
    const forked = await run(
      [
        ctx.lnxBin,
        "--instance",
        forkInstance,
        "bash",
        "-lc",
        [
          "printf forked > forked.txt",
          `if printf denied > ${cwdA}/fork-denied.txt 2>/tmp/deny.err; then echo deny-unexpected; else echo deny-ok; fi`,
        ].join("; "),
      ],
      { cwd: cwdB },
    );
    assertContains(forked.stdout, "deny-ok", "fork sibling write denied");
    assertEq(await read(join(cwdB, "forked.txt")), "forked", "fork cwd write allowed");
  });
} finally {
  await cleanupContext(ctx);
  await cleanupInstance(ctx, forkInstance);
  await cleanupDirs();
}
