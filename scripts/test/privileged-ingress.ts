import { assertContains, assertEq, cleanupContext, defaultContext, run, skip, testStep } from "./lib";

const ctx = defaultContext("privileged-ingress");

if (Bun.env.LNX_RUN_PRIVILEGED_INGRESS_TEST !== "1") {
  await skip("privileged ingress test requires LNX_RUN_PRIVILEGED_INGRESS_TEST=1 because it uses sudo, /etc/resolver, launchd, and privileged ports");
}

try {
  await cleanupContext(ctx);

  await testStep("privileged ingress install status https and disable", async () => {
    await run([
      ctx.lnxBin,
      "--instance",
      ctx.instance,
      "--no-snapshot-restore",
      "bash",
      "-lc",
      "sudo mkdir -p /tmp/lnx-privileged-ingress && printf privileged-https-ok | sudo tee /tmp/lnx-privileged-ingress/index.html >/dev/null && sudo tee /etc/systemd/system/lnx-privileged-ingress-test.service >/dev/null <<'UNIT'\n[Service]\nWorkingDirectory=/tmp/lnx-privileged-ingress\nExecStart=/usr/bin/python3 -m http.server 8080 --bind 127.0.0.1\n[Install]\nWantedBy=multi-user.target\nUNIT\nsudo systemctl daemon-reload && sudo systemctl enable --now lnx-privileged-ingress-test.service && sleep 1 && curl -fsS http://127.0.0.1:8080",
    ], { timeoutMs: 240_000 });

    const enable = await run(["sudo", ctx.lnxBin, "ingress", "enable"], { timeoutMs: 120_000 });
    assertContains(enable.stdout + enable.stderr, "ingress enabled", "ingress enable output");
    const status = await run([ctx.lnxBin, "ingress", "status"]);
    assertContains(status.stdout, "enabled", "ingress status");

    const url = `https://p8080.${ctx.instance}.lnx/`;
    const https = await run(["curl", "-fsS", "--connect-timeout", "10", "--max-time", "60", url], { timeoutMs: 90_000 });
    assertEq(https.stdout, "privileged-https-ok", "trusted https .lnx ingress");

    const disable = await run(["sudo", ctx.lnxBin, "ingress", "disable"], { timeoutMs: 120_000 });
    assertContains(disable.stdout + disable.stderr, "ingress disabled", "ingress disable output");
  });
} finally {
  await run(["sudo", ctx.lnxBin, "ingress", "disable"], { check: false, timeoutMs: 120_000 });
  await cleanupContext(ctx);
}
