import {
  mkdir,
  rm,
  writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { join } from "node:path";
import { assertEq,
  cleanupContext,
  defaultContext,
  prepareContext,
  testStep,
  waitForVmSuspend,
} from "./lib";

Bun.env.LNX_BROKER_IDLE_TTL_MS ??= "500";

const ctx = defaultContext("page-cache");
const cwd = join(ctx.repoRoot, ".lnx-page-cache");
const fileSizeMiB = 64;
const maxCacheDeltaKiB = 8192;

async function cleanupDirs() {
  await rm(cwd, { recursive: true, force: true });
}

try {
  await prepareContext(ctx);
  await cleanupDirs();
  await mkdir(cwd, { recursive: true });
  await writeFile(join(cwd, "virtiofs-cache.bin"), Buffer.alloc(fileSizeMiB * 1024 * 1024, 0x5a));

  await testStep("DAX-backed virtiofs and rootfs reads do not populate guest page cache", async () => {
    const result = await ctx.vm.cli([
        "bash",
        "-lc",
        String.raw`
set -euo pipefail

findmnt -T "$PWD" -no FSTYPE,OPTIONS | grep -q '^virtiofs .*dax=always'

python3 - <<'PY'
from pathlib import Path
path = Path("/var/tmp/local-cache.bin")
chunk = b"L" * (1024 * 1024)
with path.open("wb") as f:
    for _ in range(64):
        f.write(chunk)
PY

sync
echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null

python3 - <<'PY'
from pathlib import Path
def cache_kib():
    fields = {}
    for line in Path("/proc/meminfo").read_text().splitlines():
        key, rest = line.split(":", 1)
        fields[key] = int(rest.strip().split()[0])
    return fields["Cached"] + fields["Buffers"]
print(f"before={cache_kib()}")
PY

python3 - <<'PY'
import mmap
from pathlib import Path
def touch(path):
    total = 0
    with Path(path).open("rb") as f:
        mm = mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ)
        try:
            for offset in range(0, len(mm), 4096):
                total ^= mm[offset]
            total ^= mm[-1]
        finally:
            mm.close()
    return total
print(f"virtiofs_sum={touch('virtiofs-cache.bin')}")
print(f"local_sum={touch('/var/tmp/local-cache.bin')}")
PY

python3 - <<'PY'
from pathlib import Path
def cache_kib():
    fields = {}
    for line in Path("/proc/meminfo").read_text().splitlines():
        key, rest = line.split(":", 1)
        fields[key] = int(rest.strip().split()[0])
    return fields["Cached"] + fields["Buffers"]
print(f"after={cache_kib()}")
PY
`,
      ],
      { cwd, timeoutMs: 180_000 },
    );

    const before = Number(result.stdout.match(/^before=(\d+)$/m)?.[1]);
    const after = Number(result.stdout.match(/^after=(\d+)$/m)?.[1]);
    if (!Number.isFinite(before) || !Number.isFinite(after)) {
      throw new Error(`missing cache stats:\n${result.stdout}`);
    }
    const delta = after - before;
    if (delta > maxCacheDeltaKiB) {
      throw new Error(
        `guest page cache grew too much: before=${before}KiB after=${after}KiB delta=${delta}KiB stdout:\n${result.stdout}`,
      );
    }
    assertEq(result.stdout.includes("virtiofs_sum="), true, "virtiofs file was read");
    assertEq(result.stdout.includes("local_sum="), true, "local file was read");
    await waitForVmSuspend(ctx);
    const log = await Bun.file(join(ctx.runDir, "lnx.log")).text();
    assertEq(log.includes("snapshot.error"), false, "exit snapshot succeeded");
    assertEq(log.includes("snapshot.done"), true, "exit snapshot completed");
    assertEq(existsSync(join(ctx.snapshotDir, "latest", "vmstate.bin")), true, "snapshot vmstate exists");
  });
} finally {
  await cleanupContext(ctx);
  await cleanupDirs();
}
