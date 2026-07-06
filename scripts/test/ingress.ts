import {
  assertContains,
  assertEq,
  defaultContext,
  run,
  testStep,
} from "./lib";

const ctx = defaultContext("ingress");
const env = {
  LNX_INGRESS_DOMAIN: "lnxtest",
  LNX_INGRESS_DNS_ADDR: "127.0.0.1:15354",
  LNX_INGRESS_HTTP_ADDR: "127.0.0.1:18080",
  LNX_INGRESS_HTTPS_ADDR: "127.0.0.1:18443",
  LNX_INGRESS_RESOLVER_DIR: ctx.tmpdir,
  LNX_INGRESS_STATE_DIR: `${ctx.tmpdir}/state`,
};

function dig(host: string, ...flags: string[]) {
  return run(["dig", "-p", "15354", "@127.0.0.1", "+tries=1", "+timeout=2", ...flags, host], {
    timeoutMs: 15_000,
  });
}

try {
  await run(["rm", "-rf", ctx.tmpdir]);
  await run(["mkdir", "-p", ctx.tmpdir]);

  await testStep("unprivileged ingress lifecycle", async () => {
    await run([ctx.lnxBin, "ingress", "enable"], { env, timeoutMs: 30_000 });
    const status = await run([ctx.lnxBin, "ingress", "status"], { env });
    assertContains(status.stdout, "enabled", "ingress status");
    assertContains(status.stdout, "domain: .lnxtest", "ingress domain");
    assertContains(status.stdout, "https: 127.0.0.1:18443", "ingress https");
    assertContains(status.stdout, "protocol: ", "ingress protocol status");
    assertContains(status.stdout, "network: disabled", "ingress does not reserve per-VM addresses");
    assertEq((await run(["test", "-f", `${ctx.tmpdir}/lnxtest`], { check: false })).status, 0, "resolver exists");
    assertEq((await run(["test", "-f", `${ctx.tmpdir}/state/ca/lnx-ca.crt`], { check: false })).status, 0, "ca exists");
  });

  await testStep("dns answers port hosts and rejects unallocated instances", async () => {
    const portHost = await dig("p8080-foo.lnxtest", "+short");
    assertEq(portHost.stdout, "127.0.0.1", "port host resolves to the local proxy");
    const instance = await dig("foo.lnxtest", "+noall", "+comments");
    assertContains(instance.stdout, "status: NXDOMAIN", "unallocated instance is NXDOMAIN");
  });

  await testStep("network attach reports unavailable", async () => {
    const response = await run([
      "bash",
      "-lc",
      `printf 'POST /network/attach?instance=foo HTTP/1.1\\r\\nHost: localhost\\r\\n\\r\\n' | nc -U -w 5 ${ctx.tmpdir}/state/ingress.sock`,
    ], { timeoutMs: 15_000 });
    assertContains(response.stdout, "503 Service Unavailable", "attach is unavailable");
  });

  await testStep("unprivileged ingress disable keeps the ca", async () => {
    await run([ctx.lnxBin, "ingress", "disable"], { env, timeoutMs: 30_000 });
    const disabled = await run([ctx.lnxBin, "ingress", "status"], { env });
    assertContains(disabled.stdout, "disabled", "ingress disabled");
    assertEq((await run(["test", "-f", `${ctx.tmpdir}/state/ca/lnx-ca.crt`], { check: false })).status, 0, "ca survives disable");
  });

  await testStep("unprivileged ingress uninstall removes the ca", async () => {
    await run([ctx.lnxBin, "ingress", "uninstall"], { env, timeoutMs: 30_000 });
    assertEq((await run(["test", "-e", `${ctx.tmpdir}/state/ca`], { check: false })).status, 1, "ca dir removed");
    assertEq((await run(["test", "-e", `${ctx.tmpdir}/state/certs`], { check: false })).status, 1, "cert dir removed");
  });
} finally {
  await run([ctx.lnxBin, "ingress", "disable"], { env, check: false, timeoutMs: 30_000 });
  await run(["rm", "-rf", ctx.tmpdir], { check: false });
}
