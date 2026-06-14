import { join } from "node:path";
import {
  assertEq,
  cleanupContext,
  defaultContext,
  lnx,
  prepareContext,
  quoteShell,
  skip,
  testStep,
} from "./lib";

const fixture = Bun.env.LNX_MACOS_SNAPSHOT_FIXTURE;
if (!fixture) {
  await skip("macOS-to-Linux snapshot restore requires LNX_MACOS_SNAPSHOT_FIXTURE");
}

const ctx = defaultContext("macos-linux-snapshot");
const rootfs = Bun.env.LNX_MACOS_SNAPSHOT_ROOTFS ?? join(fixture, "rootfs.ext4");
const diskPath = Bun.env.LNX_MACOS_SNAPSHOT_DISK_PATH ?? "/root/lnx-cross-host-disk";
const memoryPath = Bun.env.LNX_MACOS_SNAPSHOT_MEMORY_PATH ?? "/run/lnx-cross-host-memory";
const expectedDisk = Bun.env.LNX_MACOS_SNAPSHOT_EXPECTED_DISK ?? "macos-disk";
const expectedMemory = Bun.env.LNX_MACOS_SNAPSHOT_EXPECTED_MEMORY ?? "macos-memory";

try {
  await prepareContext(ctx);

  await testStep("macOS snapshot restores disk and memory on Linux", async () => {
    const restored = await lnx(
      ctx,
      [
        "--no-host-shares",
        "--rootfs",
        rootfs,
        "--snapshot",
        fixture,
        "bash",
        "-lc",
        `printf "%s/%s" "$(sudo cat ${quoteShell(diskPath)})" "$(sudo cat ${quoteShell(
          memoryPath,
        )} 2>/dev/null || true)"`,
      ],
      { timeoutMs: 240_000 },
    );
    assertEq(
      restored.stdout,
      `${expectedDisk}/${expectedMemory}`,
      "macOS snapshot restored with memory",
    );
  });
} finally {
  await cleanupContext(ctx);
}
