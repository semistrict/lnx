import {
  assertContains,
  assertEq,
  cleanupContext,
  defaultContext,
  run,
  skip,
  sleep,
  spawnLnx,
  testStep,
} from "./lib";

const ctx = defaultContext("privileged-ingress");

if (Bun.env.LNX_RUN_PRIVILEGED_INGRESS_TEST !== "1") {
  await skip("privileged ingress test requires LNX_RUN_PRIVILEGED_INGRESS_TEST=1 because it uses sudo, /etc/resolver, launchd, and privileged ports");
}

async function waitForHttp(url: string, timeoutMs: number): Promise<string> {
  const deadline = Date.now() + timeoutMs;
  let last = "";
  while (Date.now() < deadline) {
    const result = await run(
      ["curl", "-fsS", "--connect-timeout", "5", "--max-time", "20", url],
      { check: false, timeoutMs: 30_000 },
    );
    if (result.status === 0) {
      return result.stdout;
    }
    last = result.stderr;
    await sleep(500);
  }
  throw new Error(`timeout waiting for ${url}: ${last}`);
}

try {
  await cleanupContext(ctx);

  // Serve a fixed body on :8080 inside a cold-booted VM, so reachability
  // depends only on the network path, not on snapshot/systemd state.
  const SERVE = "cd /tmp && printf privileged-https-ok > index.html && exec python3 -m http.server 8080 --bind 0.0.0.0";

  await testStep("ingress installs without reserving per-VM addresses", async () => {
    // Force a fresh install so the daemon runs this binary: `enable`
    // short-circuits when an ingress daemon is already loaded, which would
    // otherwise leave a stale binary serving.
    await run(["sudo", ctx.lnxBin, "ingress", "disable"], { check: false, timeoutMs: 120_000 });
    const enable = await run(["sudo", ctx.lnxBin, "ingress", "enable"], { timeoutMs: 120_000 });
    assertContains(enable.stdout + enable.stderr, "ingress enabled", "ingress enable output");
    const status = await run([ctx.lnxBin, "ingress", "status"]);
    assertContains(status.stdout, "enabled", "ingress status");
    assertContains(status.stdout, "network: disabled", "no per-VM address network reserved");
  });

  await testStep("trusted .lnx ingress proxy reachability", async () => {
    // Cold-boot a VM that serves http inline; hold it alive for every check.
    const holder = spawnLnx(ctx, ["bash", "-lc", SERVE]);
    try {
      const url = `https://p8080-${ctx.instance}.lnx/`;
      const https = await waitForHttp(url, 120_000);
      assertEq(https, "privileged-https-ok", "trusted https .lnx ingress proxy");
    } finally {
      holder.kill();
      await holder.exited;
    }
  });

  await testStep("privileged ingress disable", async () => {
    const disable = await run(["sudo", ctx.lnxBin, "ingress", "disable"], { timeoutMs: 120_000 });
    assertContains(disable.stdout + disable.stderr, "ingress disabled", "ingress disable output");
  });
} finally {
  await run(["sudo", ctx.lnxBin, "ingress", "disable"], { check: false, timeoutMs: 120_000 });
  await cleanupContext(ctx);
}
