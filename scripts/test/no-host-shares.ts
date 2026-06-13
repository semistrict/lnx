import { join } from "node:path";
import {
  assertContains,
  cleanupContext,
  defaultContext,
  lnx,
  prepareContext,
  testStep,
} from "./lib";

const ctx = defaultContext("no-host-shares");

try {
  await prepareContext(ctx);

  await testStep("guest starts without host virtiofs shares", async () => {
    await lnx(
      ctx,
      [
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

    const sharesStamp = await Bun.file(join(ctx.runDir, "shares.stamp")).text();
    assertContains(sharesStamp, "host-shares=disabled-v1", "disabled share stamp");
    assertContains(sharesStamp, "net=gvproxy", "portable network stamp");
  });
} finally {
  await cleanupContext(ctx);
}
