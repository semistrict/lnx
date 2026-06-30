import {
  assertContains,
  assertEq,
  cleanupContext,
  defaultContext,
  prepareContext,
  testStep,
} from "./lib";

const ctx = defaultContext("stock");

try {
  await prepareContext(ctx);

  await testStep("fresh stock shape", async () => {
    const shape = await ctx.vm.cli([
      "bash",
      "-lc",
      'cat /proc/1/comm; sudo readlink /proc/1/root; findmnt -n -o PROPAGATION /; if test -e /newroot || test -e /oldroot; then echo leaked; else echo clean; fi',
    ]);
    assertEq(shape.stdout, "systemd\n/\nprivate\nclean", "stock root handoff");
  });

  await testStep("stock exec user uses Ubuntu login shell", async () => {
    const shell = await ctx.vm.cli(["bash", "-lc", 'grep "^lnxuser:" /etc/passwd | cut -d: -f7']);
    assertEq(shell.stdout, "/bin/bash", "stock lnxuser login shell");
  });

  await testStep("apt works", async () => {
    await ctx.vm.cli(["sudo", "apt-get", "update"], { timeoutMs: 300_000 });
    const ruby = await ctx.vm.cli(["bash", "-lc", "apt-cache policy ruby | sed -n '1,6p'"]);
    assertContains(ruby.stdout, "Candidate:", "apt package index has ruby");
  });

  await testStep("snap hello-world works", async () => {
    await ctx.vm.cli(["bash", "-lc", "sudo DEBIAN_FRONTEND=noninteractive apt-get install -y snapd squashfs-tools"], { timeoutMs: 300_000 });
    await ctx.vm.cli(["sudo", "systemctl", "enable", "--now", "snapd.socket"]);
    await ctx.vm.cli(["bash", "-lc", "sudo systemctl start snapd.service || true"]);
    await ctx.vm.cli(["sudo", "timeout", "300s", "snap", "wait", "system", "seed.loaded"], { timeoutMs: 360_000 });
    const version = await ctx.vm.cli(["snap", "version"]);
    assertContains(version.stdout, "snapd", "snapd version");
    await ctx.vm.cli(["sudo", "snap", "install", "hello-world"], { timeoutMs: 600_000 });
    const hello = await ctx.vm.cli(["hello-world"]);
    assertContains(hello.stdout, "Hello World", "snap hello-world");
  });
} finally {
  await cleanupContext(ctx);
}
