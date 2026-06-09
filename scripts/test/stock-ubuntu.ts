import { assertContains, assertEq, cleanupContext, defaultContext, lnx, prepareContext, testStep } from "./lib";

const ctx = defaultContext("stock");

try {
  await prepareContext(ctx);

  await testStep("fresh stock shape", async () => {
    const shape = await lnx(ctx, [
      "--no-snapshot-restore",
      "bash",
      "-lc",
      'cat /proc/1/comm; sudo readlink /proc/1/root; findmnt -n -o PROPAGATION /; if test -e /newroot || test -e /oldroot; then echo leaked; else echo clean; fi',
    ]);
    assertEq(shape.stdout, "systemd\n/\nprivate\nclean", "stock root handoff");
  });

  await testStep("apt works", async () => {
    await lnx(ctx, ["sudo", "apt-get", "update"], { timeoutMs: 300_000 });
    const ruby = await lnx(ctx, ["bash", "-lc", "apt-cache policy ruby | sed -n '1,6p'"]);
    assertContains(ruby.stdout, "Candidate:", "apt package index has ruby");
  });

  await testStep("snap hello-world works", async () => {
    await lnx(ctx, ["bash", "-lc", "sudo DEBIAN_FRONTEND=noninteractive apt-get install -y snapd squashfs-tools"], { timeoutMs: 300_000 });
    await lnx(ctx, ["sudo", "systemctl", "enable", "--now", "snapd.socket"]);
    await lnx(ctx, ["bash", "-lc", "sudo systemctl start snapd.service || true"]);
    const version = await lnx(ctx, ["snap", "version"]);
    assertContains(version.stdout, "snapd", "snapd version");
    await lnx(ctx, ["sudo", "snap", "install", "hello-world"], { timeoutMs: 600_000 });
    const hello = await lnx(ctx, ["hello-world"]);
    assertContains(hello.stdout, "Hello World", "snap hello-world");
  });
} finally {
  await cleanupContext(ctx);
}
