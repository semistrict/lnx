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

  await testStep("privileged ingress install status https and disable", async () => {
    await run([
      ctx.lnxBin,
      "--instance",
      ctx.instance,
      "--no-snapshot-restore",
      "bash",
      "-lc",
      "sudo mkdir -p /tmp/lnx-privileged-ingress && printf privileged-https-ok | sudo tee /tmp/lnx-privileged-ingress/index.html >/dev/null && sudo tee /etc/systemd/system/lnx-privileged-ingress-test.service >/dev/null <<'UNIT'\n[Service]\nWorkingDirectory=/tmp/lnx-privileged-ingress\nExecStart=/usr/bin/python3 -m http.server 8080 --bind 0.0.0.0\n[Install]\nWantedBy=multi-user.target\nUNIT\nsudo systemctl daemon-reload && sudo systemctl enable --now lnx-privileged-ingress-test.service && sleep 1 && curl -fsS http://127.0.0.1:8080",
    ], { timeoutMs: 240_000 });

    const enable = await run(["sudo", ctx.lnxBin, "ingress", "enable"], { timeoutMs: 120_000 });
    assertContains(enable.stdout + enable.stderr, "ingress enabled", "ingress enable output");
    const status = await run([ctx.lnxBin, "ingress", "status"]);
    assertContains(status.stdout, "enabled", "ingress status");
    assertContains(status.stdout, "network: 192.168.106.0/24", "vmnet network reserved");

    const url = `https://p8080-${ctx.instance}.lnx/`;
    const https = await run(["curl", "-fsS", "--connect-timeout", "10", "--max-time", "60", url], { timeoutMs: 90_000 });
    assertEq(https.stdout, "privileged-https-ok", "trusted https .lnx ingress");
  });

  await testStep("instance gets a routable vmnet address and DNS name", async () => {
    // Keep the VM alive: direct-to-IP traffic has no L7 hook to auto-start
    // the instance the way the ingress proxy does.
    const holder = spawnLnx(ctx, ["bash", "-lc", "sleep 120"]);
    try {
      const log = await run(["bash", "-lc", `cat ${join(ctx.runDir, "lnx.log")}`]);
      assertContains(log.stdout, "network.vmnet ip=192.168.106.", "owner attached to vmnet");

      const resolved = await run(
        ["dig", "-p", "5354", "@127.0.0.1", "+short", `${ctx.instance}.lnx`],
        { timeoutMs: 15_000 },
      );
      assertContains(resolved.stdout, "192.168.106.", "instance name resolves to its address");

      // System resolver -> /etc/resolver/lnx -> ingress DNS -> direct VM IP.
      const direct = await waitForHttp(`http://${ctx.instance}.lnx:8080/`, 60_000);
      assertEq(direct, "privileged-https-ok", "host reaches the VM by name and IP");
    } finally {
      holder.kill();
      await holder.exited;
    }
  });

  await testStep("VMs reach each other by name", async () => {
    const holder = spawnLnx(ctx, ["bash", "-lc", "sleep 240"]);
    try {
      const peer = await run([
        ctx.lnxBin,
        "--instance",
        peerInstance,
        "bash",
        "-lc",
        `curl -fsS --connect-timeout 10 --max-time 30 http://${ctx.instance}.lnx:8080/`,
      ], { timeoutMs: 600_000 });
      assertEq(peer.stdout, "privileged-https-ok", "guest resolves and reaches its peer");
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
