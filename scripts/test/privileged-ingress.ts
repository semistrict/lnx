import { join } from "node:path";
import {
  assertContains,
  assertEq,
  cleanupContext,
  cleanupInstance,
  defaultContext,
  run,
  skip,
  sleep,
  spawnLnx,
  testStep,
} from "./lib";

const ctx = defaultContext("privileged-ingress");
const peerInstance = `${ctx.instance}-peer`;

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
  await cleanupInstance(ctx, peerInstance);

  // Serve a fixed body on :8080 inside a cold-booted VM, so reachability
  // depends only on the network path, not on snapshot/systemd state.
  const SERVE = "cd /tmp && printf privileged-https-ok > index.html && exec python3 -m http.server 8080 --bind 0.0.0.0";

  await testStep("ingress installs and reserves a vmnet network", async () => {
    // Force a fresh install so the daemon runs this binary: `enable`
    // short-circuits when an ingress daemon is already loaded, which would
    // otherwise leave a stale binary (without vmnet) serving.
    await run(["sudo", ctx.lnxBin, "ingress", "disable"], { check: false, timeoutMs: 120_000 });
    const enable = await run(["sudo", ctx.lnxBin, "ingress", "enable"], { timeoutMs: 120_000 });
    assertContains(enable.stdout + enable.stderr, "ingress enabled", "ingress enable output");
    const status = await run([ctx.lnxBin, "ingress", "status"]);
    assertContains(status.stdout, "enabled", "ingress status");
    assertContains(status.stdout, "network: 192.168.106.0/24", "vmnet network reserved");
  });

  async function holderVmnetIp(timeoutMs = 120_000): Promise<string> {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      const log = await run(["bash", "-lc", `cat ${join(ctx.runDir, "lnx.log")} 2>/dev/null || true`]);
      const match = log.stdout.match(/network\.vmnet ip=(\d+\.\d+\.\d+\.\d+)\//);
      if (match) {
        return match[1];
      }
      await sleep(500);
    }
    throw new Error("owner never attached to vmnet (no network.vmnet ip= in lnx.log)");
  }

  await testStep("routable vmnet address, DNS name, and reachability", async () => {
    // Cold-boot a VM that serves http inline; hold it alive for every check.
    const holder = spawnLnx(ctx, ["bash", "-lc", SERVE]);
    try {
      const ip = await holderVmnetIp();
      // Reach the routable address directly (host -> vmnet bridge -> VM).
      const direct = await waitForHttp(`http://${ip}:8080/`, 120_000);
      assertEq(direct, "privileged-https-ok", "host reaches the VM by its routable IP");

      // <instance>.lnx resolves to the routable address (host-side DNS).
      const resolved = await run(
        ["dig", "-p", "5354", "@127.0.0.1", "+short", `${ctx.instance}.lnx`],
        { timeoutMs: 15_000 },
      );
      assertEq(resolved.stdout, ip, "instance name resolves to its address");

      // Host reaches the VM by name: system resolver -> ingress DNS -> IP.
      const byName = await waitForHttp(`http://${ctx.instance}.lnx:8080/`, 30_000);
      assertEq(byName, "privileged-https-ok", "host reaches the VM by name");

      // The L7 ingress proxy terminates TLS and forwards over the broker.
      const url = `https://p8080-${ctx.instance}.lnx/`;
      const https = await run(
        ["curl", "-fsS", "--connect-timeout", "10", "--max-time", "30", url],
        { timeoutMs: 60_000 },
      );
      assertEq(https.stdout, "privileged-https-ok", "trusted https .lnx ingress proxy");

      // A peer on the shared L2 segment reaches the holder by its address.
      const peer = await run([
        ctx.lnxBin,
        "--instance",
        peerInstance,        "bash",
        "-lc",
        `curl -fsS --connect-timeout 10 --max-time 30 http://${ip}:8080/`,
      ], { timeoutMs: 600_000 });
      assertEq(peer.stdout, "privileged-https-ok", "peer reaches the holder by IP");
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
  await cleanupInstance(ctx, peerInstance);
  await cleanupContext(ctx);
}
