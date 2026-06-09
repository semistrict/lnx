import { defaultContext, run, skip, testStep, assertContains } from "./lib";

const ctx = defaultContext("privileged-ingress");

if (Bun.env.LNX_RUN_PRIVILEGED_INGRESS_TEST !== "1") {
  await skip("privileged ingress test requires LNX_RUN_PRIVILEGED_INGRESS_TEST=1 because it uses sudo, /etc/resolver, launchd, and privileged ports");
}

try {
  await testStep("privileged ingress install status and disable", async () => {
    const enable = await run(["sudo", ctx.lnxBin, "ingress", "enable"], { timeoutMs: 120_000 });
    assertContains(enable.stdout + enable.stderr, "ingress enabled", "ingress enable output");
    const status = await run([ctx.lnxBin, "ingress", "status"]);
    assertContains(status.stdout, "enabled", "ingress status");
    const disable = await run(["sudo", ctx.lnxBin, "ingress", "disable"], { timeoutMs: 120_000 });
    assertContains(disable.stdout + disable.stderr, "ingress disabled", "ingress disable output");
  });
} finally {
  await run(["sudo", ctx.lnxBin, "ingress", "disable"], { check: false, timeoutMs: 120_000 });
}
