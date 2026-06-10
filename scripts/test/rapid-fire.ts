import { existsSync } from "node:fs";
import { join } from "node:path";
import {
  assertEq,
  cleanupContext,
  defaultContext,
  lnx,
  prepareContext,
  testStep,
  waitForVmSuspend,
} from "./lib";

// This test exercises the default idle grace period, so it must not inherit a
// shortened TTL from the environment.
delete Bun.env.LNX_BROKER_IDLE_TTL_MS;

const ctx = defaultContext("rapid-fire");

try {
  await prepareContext(ctx);

  await testStep("client exits before the post-command snapshot", async () => {
    assertEq((await lnx(ctx, ["--no-snapshot-restore", "echo", "cold"])).stdout, "cold", "cold exec");
    assertEq(existsSync(join(ctx.snapshotDir, "latest", "vmstate.bin")), false, "snapshot deferred past client exit");
    assertEq(existsSync(join(ctx.runDir, "broker.sock")), true, "broker stays up for the grace period");
  });

  await testStep("rapid-fire commands reuse the live VM", async () => {
    await lnx(ctx, ["bash", "-lc", "echo marker > /tmp/rapid-fire"]);
    assertEq((await lnx(ctx, ["cat", "/tmp/rapid-fire"])).stdout, "marker", "tmpfs state survives between commands");
    const failed = await lnx(ctx, ["bash", "-lc", "echo stdout-line; echo stderr-line >&2; exit 7"], { check: false });
    assertEq(failed.status, 7, "exit status through the live broker");
    assertEq(failed.stdout, "stdout-line", "stdout through the live broker");
    assertEq(failed.stderr, "stderr-line", "stderr through the live broker");
    assertEq((await lnx(ctx, ["cat"], { stdin: "stdin-ok" })).stdout, "stdin-ok", "stdin through the live broker");
  });

  await testStep("lnxctl snapshot-exit works against the live broker", async () => {
    assertEq((await lnx(ctx, ["lnxctl", "snapshot-exit"])).status, 0, "lnxctl snapshot-exit status");
    assertEq((await lnx(ctx, ["echo", "post-lnxctl"])).stdout, "post-lnxctl", "exec after lnxctl snapshot-exit");
  });

  await testStep("idle VM suspends after the grace period", async () => {
    await waitForVmSuspend(ctx, 120_000);
    assertEq(existsSync(join(ctx.runDir, "broker.sock")), false, "broker exits after the grace period");
    assertEq(existsSync(join(ctx.snapshotDir, "latest", "vmstate.bin")), true, "suspend wrote the snapshot");
    assertEq((await lnx(ctx, ["cat", "/tmp/rapid-fire"])).stdout, "marker", "snapshot captured pre-suspend state");
    await waitForVmSuspend(ctx, 120_000);
  });
} finally {
  await cleanupContext(ctx);
}
