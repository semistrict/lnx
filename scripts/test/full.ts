const tests = [
  "system",
  "cp",
  "checkpoint-fork",
  "fork-fanout",
  "snapshot-compat",
  "dirty-fs",
  "broker-recovery",
  "client-chaos",
  "pty-resume",
  "stress",
  "stock-ubuntu",
  "ingress",
  "browser-snapshot",
  "privileged-ingress",
];

for (const test of tests) {
  const proc = Bun.spawn(["bun", `scripts/test/${test}.ts`], {
    env: Bun.env,
    stdout: "inherit",
    stderr: "inherit",
  });
  const status = await proc.exited;
  if (status !== 0) {
    throw new Error(`${test} failed with status ${status}`);
  }
}
