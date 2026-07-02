import {
  existsSync } from "node:fs";
import { mkdir,
  rm } from "node:fs/promises";
import { join } from "node:path";
import {
  assertContains,
  assertEq,
  cloneSparseImage,
  cleanupContext,
  defaultContext,
  prepareContext,
  quoteShell,
  run,
  skip,
  startHostHttpProbe,
  testStep,
  waitForVmSuspend,
} from "./lib";

if (process.platform !== "darwin") {
  await skip("Linux-to-macOS snapshot restore requires a macOS host");
}

const fixture = Bun.env.LNX_LINUX_SNAPSHOT_FIXTURE;
if (!fixture) {
  await skip("Linux-to-macOS snapshot restore requires LNX_LINUX_SNAPSHOT_FIXTURE");
}

const ctx = defaultContext("linux-macos-snapshot");
const rootfs = Bun.env.LNX_LINUX_SNAPSHOT_ROOTFS ?? join(fixture, "rootfs.ext4");
const diskPath = Bun.env.LNX_LINUX_SNAPSHOT_DISK_PATH ?? "/root/lnx-cross-host-disk";
const memoryPath = Bun.env.LNX_LINUX_SNAPSHOT_MEMORY_PATH ?? "/run/lnx-cross-host-memory";
const afterPath = Bun.env.LNX_LINUX_SNAPSHOT_AFTER_PATH ?? "/root/lnx-cross-host-after";
const expectedDisk = Bun.env.LNX_LINUX_SNAPSHOT_EXPECTED_DISK ?? "linux-disk";
const expectedMemory = Bun.env.LNX_LINUX_SNAPSHOT_EXPECTED_MEMORY ?? "linux-memory";
const expectedAfter = Bun.env.LNX_LINUX_SNAPSHOT_EXPECTED_AFTER ?? "linux-after";
const exportMacosSnapshot = Bun.env.LNX_LINUX_MACOS_EXPORT_MACOS_SNAPSHOT;

try {
  await prepareContext(ctx);

  await testStep("Linux snapshot restores disk and memory on macOS", async () => {
    const hostProbe = startHostHttpProbe("linux-macos");
    const vmstate = await Bun.file(join(fixture, "vmstate.bin")).arrayBuffer();
    assertEq(new DataView(vmstate).getUint32(8, true), 4, "source snapshot is shared vmstate v4");

    try {
      const restored = await ctx.vm.cli([
          "--no-host-shares",
          "--memory-mib",
          "512",
          "--cpus",
          "1",
          "--rootfs",
          rootfs,
          "--snapshot",
          fixture,
          "bash",
          "-lc",
          [
            "set -euo pipefail",
            "sudo sh -c 'printf go >/run/lnx-cross-host-go'",
            `for i in $(seq 1 1200); do sudo test -f ${quoteShell(afterPath)} && break; sleep 0.1; done`,
            `sudo test -f ${quoteShell(afterPath)}`,
            `probe="$(curl --noproxy '*' -fsS --max-time 20 ${quoteShell(hostProbe.guestUrl("192.168.127.254"))} | tr -d '\\n')"`,
            `test "$probe" = ${quoteShell(hostProbe.expected)}`,
            `printf "%s/%s/%s/%s" "$(sudo cat ${quoteShell(diskPath)})" "$(sudo cat ${quoteShell(
              memoryPath,
            )} 2>/dev/null || true)" "$(sudo cat ${quoteShell(afterPath)})" "mac-host-probe"`,
            ...(exportMacosSnapshot ? ["lnxctl snapshot-exit >/dev/null"] : []),
          ].join("; "),
        ],
        {
          timeoutMs: 300_000,
          env: {
            LNX_BROKER_IDLE_TTL_MS: "250",
            LNX_INGRESS_STATE_DIR: join(ctx.tmpdir, "disabled-ingress"),
          },
        },
      );
      assertEq(
        restored.stdout,
        `${expectedDisk}/${expectedMemory}/${expectedAfter}/mac-host-probe`,
        "Linux snapshot restored with memory on macOS",
      );

      if (exportMacosSnapshot) {
        await waitForVmSuspend(ctx, 120_000);
        const latest = join(ctx.snapshotDir, "latest");
        for (const file of ["vmstate.bin", "pages.img", "rootfs.ext4", "launch.json", "initramfs.stamp"]) {
          const path = join(latest, file);
          if (!existsSync(path)) {
            throw new Error(`missing exported macOS snapshot file: ${path}`);
          }
        }
        await rm(exportMacosSnapshot, { recursive: true, force: true });
        await mkdir(exportMacosSnapshot, { recursive: true });
        await cloneSparseImage(join(latest, "rootfs.ext4"), join(exportMacosSnapshot, "rootfs.ext4"));
        await cloneSparseImage(join(latest, "pages.img"), join(exportMacosSnapshot, "pages.img"));
        for (const file of ["vmstate.bin", "launch.json", "initramfs.stamp"]) {
          await run(["cp", join(latest, file), join(exportMacosSnapshot, file)], { timeoutMs: 180_000 });
        }
        console.log(`LNX_MACOS_SNAPSHOT_FIXTURE=${exportMacosSnapshot}`);
      }
    } finally {
      hostProbe.stop();
    }
  });
} finally {
  await cleanupContext(ctx);
}
