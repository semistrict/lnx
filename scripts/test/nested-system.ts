import { assertContains, assertEq, cleanupContext, defaultContext, lnx, prepareContext, run, testStep } from "./lib";

const ctx = defaultContext("nested-system");

try {
  await prepareContext(ctx);

  await testStep("paths and init commands work on a Linux host", async () => {
    const paths = await run([ctx.lnxBin, "--instance", ctx.instance, "paths"]);
    assertContains(paths.stdout, `name: ${ctx.instance}`, "paths prints instance name");
    assertContains(paths.stdout, `rootfs: ${ctx.imageDir}/rootfs.ext4`, "paths prints rootfs");
    assertContains(paths.stdout, `snapshots: ${ctx.snapshotDir}`, "paths prints snapshots");

    const envPaths = await run([ctx.lnxBin, "paths"], { env: { LNX_INSTANCE: ctx.instance } });
    assertContains(envPaths.stdout, `name: ${ctx.instance}`, "paths honors LNX_INSTANCE");
  });

  await testStep("basic exec and guest shape work on a Linux host", async () => {
    assertEq((await lnx(ctx, ["--no-snapshot-restore", "echo", "cold"])).stdout, "cold", "cold exec");
    assertEq((await lnx(ctx, ["run", "echo", "run-subcommand"])).stdout, "run-subcommand", "run subcommand exec");
    assertEq((await lnx(ctx, ["cat"], { stdin: "stdin-ok" })).stdout, "stdin-ok", "non-pty stdin");
    assertEq((await lnx(ctx, [], { stdin: "echo noargs-shell; exit\n" })).stdout, "noargs-shell", "default shell over stdin");
    const failed = await lnx(ctx, ["bash", "-lc", "echo stdout-line; echo stderr-line >&2; exit 7"], { check: false });
    assertEq(failed.status, 7, "exit status propagation");
    assertEq(failed.stdout, "stdout-line", "stdout propagation");
    assertEq(failed.stderr, "stderr-line", "stderr propagation");
    const notFound = await lnx(ctx, ["definitely-not-a-command"], { check: false });
    assertEq(notFound.status, 127, "command-not-found status");
    assertContains(notFound.stderr, "exec failed", "command-not-found stderr");

    assertEq((await lnx(ctx, ["id", "-un"])).stdout, "lnxuser", "exec runs as lnxuser");
    assertEq((await lnx(ctx, ["bash", "-lc", 'printf "%s:%s:%s" "$USER" "$LOGNAME" "$HOME"'])).stdout, "lnxuser:lnxuser:/home/lnxuser", "exec user environment");
    assertEq((await lnx(ctx, ["getconf", "PAGESIZE"])).stdout, "16384", "guest page size");
    assertContains((await lnx(ctx, ["bash", "-lc", "printf %s \"$PATH\""])).stdout, "/snap/bin", "exec PATH includes snap commands");
    const pid1 = await lnx(ctx, ["bash", "-lc", "cat /proc/1/comm; sudo readlink /proc/1/root; test ! -e /newroot; test ! -e /oldroot; echo clean"]);
    assertEq(pid1.stdout, "systemd\n/\nclean", "systemd owns final root");
    const rootMount = await lnx(ctx, ["findmnt", "-n", "-o", "FSTYPE,OPTIONS", "/"]);
    assertContains(rootMount.stdout, "ext4", "root is ext4");
  });

  await testStep("network and lnxctl help work on a Linux host", async () => {
    const probe = await lnx(ctx, [
      "bash",
      "-lc",
      'tmp=/tmp/lnx-network-probe; rm -f "$tmp"; curl -fsS --max-time 20 -o "$tmp" http://ports.ubuntu.com/ubuntu-ports/dists/resolute/InRelease; sed -n "1p" "$tmp"',
    ]);
    assertEq(probe.stdout, "-----BEGIN PGP SIGNED MESSAGE-----", "outbound networking");

    const help = await lnx(ctx, ["lnxctl", "--help"], { check: false });
    assertEq(help.status, 2, "lnxctl usage status");
    assertContains(help.stderr, "usage: lnxctl snapshot-exit", "lnxctl usage text");
  });
} finally {
  await cleanupContext(ctx);
}
