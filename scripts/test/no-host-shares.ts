import {
  join } from "node:path";
import {
  assertContains,
  assertEq,
  cleanupContext,
  defaultContext,
  prepareContext,
  testStep,
} from "./lib";

const ctx = defaultContext("no-host-shares");

try {
  await prepareContext(ctx);

  await testStep("guest starts without host virtiofs shares", async () => {
    await ctx.vm.cli([
        "--no-host-shares",
        "bash",
        "-lc",
        [
          "set -euo pipefail",
          'test "$(pwd)" = /',
          'test -z "${LNX_VIRTIOFS_HOME:-}"',
          'test -z "${LNX_VIRTIOFS_CWD:-}"',
          "! grep -qw virtiofs /proc/mounts",
        ].join("; "),
      ],
      {
        timeoutMs: 180_000,
        env: {
          LNX_INGRESS_STATE_DIR: join(ctx.tmpdir, "disabled-ingress"),
        },
      },
    );

    const log = await Bun.file(join(ctx.runDir, "lnx.log")).text();
    assertContains(log, "host_shares.disabled", "host shares disabled log line");

    const launch = JSON.parse(await Bun.file(join(ctx.runDir, "launch.json")).text());
    assertEq(launch.shares.no_host_shares, true, "host shares disabled in launch metadata");
    assertEq("net" in launch.compatibility, false, "network is not a launch option");
  });
} finally {
  await cleanupContext(ctx);
}
