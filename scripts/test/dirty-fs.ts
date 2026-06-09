import { existsSync } from "node:fs";
import { join } from "node:path";
import { assertEq, cleanupContext, defaultContext, lnx, prepareContext, run, skip, testStep } from "./lib";

const ctx = defaultContext("dirty-fs");
const forkName = `${ctx.instance}-dirty-fork`;
const e2fsck =
  ["/opt/homebrew/opt/e2fsprogs/sbin/e2fsck", "/usr/local/opt/e2fsprogs/sbin/e2fsck", "e2fsck"].find((candidate) =>
    candidate.includes("/") ? existsSync(candidate) : true,
  ) ?? "e2fsck";

try {
  if (!e2fsck.includes("/") && (await run(["bash", "-lc", "command -v e2fsck >/dev/null"], { check: false })).status !== 0) {
    await skip("dirty filesystem test requires host e2fsck");
  }
  await prepareContext(ctx);
  await run(["rm", "-rf", join(ctx.base, "images", forkName), join(ctx.base, "instances", forkName)], { check: false });

  await testStep("write dirty workload and checkpoint", async () => {
    await lnx(ctx, [
      "--no-snapshot-restore",
      "bash",
      "-lc",
      "rm -rf /root/dirty; mkdir /root/dirty; for i in $(seq 1 250); do printf file-$i >/root/dirty/file-$i; done; sync",
    ]);
    assertEq((await run([ctx.lnxBin, "--instance", ctx.instance, "checkpoint", "-m", "dirty"])).stdout, "dirty", "dirty checkpoint");
    assertEq((await run([ctx.lnxBin, "--instance", ctx.instance, "fork", "--checkpoint", "dirty", forkName])).stdout, forkName, "dirty fork");
  });

  await testStep("offline repair fsck of checkpoint and fork rootfs clones", async () => {
    for (const rootfs of [
      join(ctx.imageDir, "checkpoints"),
      join(ctx.base, "images", forkName, "rootfs.ext4"),
    ]) {
      const target = rootfs.endsWith("checkpoints")
        ? (await run(["bash", "-lc", `find ${rootfs} -name rootfs.ext4 | head -n1`])).stdout
        : rootfs;
      const clone = join(ctx.tmpdir, `fsck-${target.split("/").at(-3) ?? "rootfs"}.ext4`);
      await run(["cp", "-c", target, clone], { timeoutMs: 120_000 });
      const fsck = await run([e2fsck, "-fy", clone], { check: false, timeoutMs: 120_000 });
      const output = fsck.stdout + fsck.stderr;
      assertEq(fsck.status === 0 || fsck.status === 1, true, `fsck repair status for ${target}: ${output}`);
      assertEq(output.includes("UNEXPECTED INCONSISTENCY"), false, `fsck unexpected inconsistency for ${target}`);
    }
  });
} finally {
  await cleanupContext(ctx);
  await run(["rm", "-rf", join(ctx.base, "images", forkName), join(ctx.base, "instances", forkName)], { check: false });
}
