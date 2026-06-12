import { join } from "node:path";
import {
  assertContains,
  assertEq,
  assertFile,
  cleanupContext,
  defaultContext,
  lnx,
  prepareContext,
  read,
  run,
  testStep,
  waitForVmSuspend,
  write,
} from "./lib";

// Shorten the detached owner's idle grace period so suspend-dependent
// assertions don't wait the full default between steps.
Bun.env.LNX_BROKER_IDLE_TTL_MS ??= "500";

const ctx = defaultContext("system");
const copyInstance = `${ctx.instance}-copy`;
const copyImageDir = join(ctx.base, "images", copyInstance);
const copyRunDir = join(ctx.base, "instances", copyInstance);

try {
  await prepareContext(ctx);
  await run(["rm", "-rf", copyImageDir, copyRunDir]);

  await testStep("paths and init", async () => {
    const paths = await run([ctx.lnxBin, "--instance", ctx.instance, "paths"]);
    assertContains(paths.stdout, `name: ${ctx.instance}`, "paths prints instance name");
    assertContains(paths.stdout, `rootfs: ${ctx.imageDir}/rootfs.ext4`, "paths prints rootfs");
    assertContains(paths.stdout, `snapshots: ${ctx.snapshotDir}`, "paths prints snapshots");

    const envPaths = await run([ctx.lnxBin, "paths"], { env: { LNX_INSTANCE: ctx.instance } });
    assertContains(envPaths.stdout, `name: ${ctx.instance}`, "paths honors LNX_INSTANCE");

    await write(join(ctx.tmpdir, "kernel"), "kernel-copy-test");
    await write(join(ctx.tmpdir, "rootfs.ext4"), "rootfs-copy-test");
    await run([
      ctx.lnxBin,
      "--instance",
      copyInstance,
      "--kernel",
      join(ctx.tmpdir, "copied-kernel"),
      "--rootfs",
      join(ctx.tmpdir, "copied-rootfs.ext4"),
      "init",
      "--kernel",
      join(ctx.tmpdir, "kernel"),
      "--rootfs",
      join(ctx.tmpdir, "rootfs.ext4"),
    ]);
    assertEq(await read(join(ctx.tmpdir, "copied-kernel")), "kernel-copy-test", "explicit init copied kernel");
    assertEq(await read(join(ctx.tmpdir, "copied-rootfs.ext4")), "rootfs-copy-test", "explicit init copied rootfs");
  });

  await testStep("basic exec and snapshots", async () => {
    assertEq((await lnx(ctx, ["--no-snapshot-restore", "echo", "cold"])).stdout, "cold", "cold exec");
    assertFile(join(ctx.base, "vmlinuz"), "auto-init kernel");
    assertFile(join(ctx.imageDir, "rootfs.ext4"), "auto-init rootfs");
    await waitForVmSuspend(ctx);
    assertFile(join(ctx.snapshotDir, "latest", "vmstate.bin"), "full snapshot vmstate");
    assertFile(join(ctx.snapshotDir, "latest", "pages.img"), "full snapshot pages");
    assertFile(join(ctx.snapshotDir, "latest", "rootfs.ext4"), "full snapshot rootfs");
    assertEq((await lnx(ctx, ["echo", "restored"])).stdout, "restored", "restored exec");
    assertEq((await lnx(ctx, ["run", "echo", "run-subcommand"])).stdout, "run-subcommand", "run subcommand exec");
    await waitForVmSuspend(ctx);
    assertEq((await lnx(ctx, ["--snapshot", join(ctx.snapshotDir, "latest"), "echo", "explicit-snapshot"])).stdout, "explicit-snapshot", "explicit snapshot restore");
  });

  await testStep("stdio and status", async () => {
    assertEq((await lnx(ctx, ["cat"], { stdin: "stdin-ok" })).stdout, "stdin-ok", "non-pty stdin");
    assertEq((await lnx(ctx, [], { stdin: "echo noargs-shell; exit\n" })).stdout, "noargs-shell", "default shell over stdin");
    const failed = await lnx(ctx, ["bash", "-lc", "echo stdout-line; echo stderr-line >&2; exit 7"], { check: false });
    assertEq(failed.status, 7, "exit status propagation");
    assertEq(failed.stdout, "stdout-line", "stdout propagation");
    assertEq(failed.stderr, "stderr-line", "stderr propagation");
    const notFound = await lnx(ctx, ["definitely-not-a-command"], { check: false });
    assertEq(notFound.status, 127, "command-not-found status");
    assertContains(notFound.stderr, "exec failed", "command-not-found stderr");
  });

  await testStep("guest shape", async () => {
    assertEq((await lnx(ctx, ["id", "-un"])).stdout, "lnxuser", "exec runs as lnxuser");
    assertEq((await lnx(ctx, ["bash", "-lc", 'printf "%s:%s:%s" "$USER" "$LOGNAME" "$HOME"'])).stdout, "lnxuser:lnxuser:/home/lnxuser", "exec user environment");
    assertContains((await lnx(ctx, ["bash", "-lc", "id -u; id -g"])).stdout, `${process.getuid?.() ?? 0}\n${process.getgid?.() ?? 0}`, "exec uid/gid match host");
    const hostGroup = (await run(["id", "-gn"])).stdout;
    assertEq((await lnx(ctx, ["id", "-gn"])).stdout, hostGroup, "exec primary group named like host");
    assertEq((await lnx(ctx, ["getconf", "PAGESIZE"])).stdout, "16384", "guest page size");
    assertEq((await lnx(ctx, ["nproc"])).stdout, "2", "default cpu count");
    assertContains((await lnx(ctx, ["bash", "-lc", "printf %s \"$PATH\""])).stdout, "/snap/bin", "exec PATH includes snap commands");
    const pid1 = await lnx(ctx, ["bash", "-lc", "cat /proc/1/comm; sudo readlink /proc/1/root; test ! -e /newroot; test ! -e /oldroot; echo clean"]);
    assertEq(pid1.stdout, "systemd\n/\nclean", "systemd owns final root");
    const rootMount = await lnx(ctx, ["findmnt", "-n", "-o", "FSTYPE,OPTIONS", "/"]);
    assertContains(rootMount.stdout, "ext4", "root is ext4");
    assertContains(rootMount.stdout, "dax=always", "root is dax mounted");
  });

  await testStep("network and lnxctl", async () => {
    const probe = await lnx(ctx, [
      "bash",
      "-lc",
      'tmp=/tmp/lnx-network-probe; rm -f "$tmp"; curl -fsS --max-time 20 -o "$tmp" http://ports.ubuntu.com/ubuntu-ports/dists/resolute/InRelease; sed -n "1p" "$tmp"',
    ]);
    assertEq(probe.stdout, "-----BEGIN PGP SIGNED MESSAGE-----", "outbound networking");

    const help = await lnx(ctx, ["lnxctl", "--help"], { check: false });
    assertEq(help.status, 2, "lnxctl usage status");
    assertContains(help.stderr, "usage: lnxctl snapshot-exit", "lnxctl usage text");
    assertEq((await lnx(ctx, ["lnxctl", "snapshot-exit"])).status, 0, "lnxctl snapshot-exit status");
    assertEq((await lnx(ctx, ["echo", "post-lnxctl"])).stdout, "post-lnxctl", "exec after lnxctl snapshot-exit");
  });
} finally {
  await cleanupContext(ctx);
  await run(["rm", "-rf", copyImageDir, copyRunDir], { check: false });
}
