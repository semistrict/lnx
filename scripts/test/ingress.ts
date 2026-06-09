import { assertContains, assertEq, defaultContext, run, testStep } from "./lib";

const ctx = defaultContext("ingress");
const env = {
  LNX_INGRESS_DOMAIN: "lnxtest",
  LNX_INGRESS_DNS_ADDR: "127.0.0.1:15354",
  LNX_INGRESS_HTTP_ADDR: "127.0.0.1:18080",
  LNX_INGRESS_HTTPS_ADDR: "127.0.0.1:18443",
  LNX_INGRESS_RESOLVER_DIR: ctx.tmpdir,
  LNX_INGRESS_STATE_DIR: `${ctx.tmpdir}/state`,
};

try {
  await run(["rm", "-rf", ctx.tmpdir]);
  await run(["mkdir", "-p", ctx.tmpdir]);

  await testStep("unprivileged ingress lifecycle", async () => {
    await run([ctx.lnxBin, "ingress", "enable"], { env, timeoutMs: 30_000 });
    const status = await run([ctx.lnxBin, "ingress", "status"], { env });
    assertContains(status.stdout, "enabled", "ingress status");
    assertContains(status.stdout, "domain: .lnxtest", "ingress domain");
    assertContains(status.stdout, "https: 127.0.0.1:18443", "ingress https");
    assertEq((await run(["test", "-f", `${ctx.tmpdir}/lnxtest`], { check: false })).status, 0, "resolver exists");
    assertEq((await run(["test", "-f", `${ctx.tmpdir}/state/ca/lnx-ca.crt`], { check: false })).status, 0, "ca exists");
    await run([ctx.lnxBin, "ingress", "disable"], { env, timeoutMs: 30_000 });
    const disabled = await run([ctx.lnxBin, "ingress", "status"], { env });
    assertContains(disabled.stdout, "disabled", "ingress disabled");
  });
} finally {
  await run([ctx.lnxBin, "ingress", "disable"], { env, check: false, timeoutMs: 30_000 });
  await run(["rm", "-rf", ctx.tmpdir], { check: false });
}
