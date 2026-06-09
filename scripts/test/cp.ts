import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { assertEq, cleanupContext, defaultContext, lnx, prepareContext, run, testStep } from "./lib";

const ctx = defaultContext("cp");

try {
  await prepareContext(ctx);
  await mkdir(join(ctx.tmpdir, "host-src", "dir"), { recursive: true });
  await mkdir(join(ctx.tmpdir, "host-out"), { recursive: true });
  await writeFile(join(ctx.tmpdir, "host-src", "hello.txt"), "hello from host\n");
  await writeFile(join(ctx.tmpdir, "host-src", "dir", "nested.txt"), "nested from host\n");

  await testStep("host to guest copies files and directories with -R", async () => {
    await run([
      ctx.lnxBin,
      "--instance",
      ctx.instance,
      "cp",
      "-R",
      `host:${join(ctx.tmpdir, "host-src", "hello.txt")}`,
      `host:${join(ctx.tmpdir, "host-src", "dir")}`,
      "/root/copied-from-host",
    ]);
    const result = await lnx(ctx, [
      "bash",
      "-lc",
      'printf "%s/%s" "$(cat /root/copied-from-host/hello.txt)" "$(cat /root/copied-from-host/dir/nested.txt)"',
    ]);
    assertEq(result.stdout, "hello from host/nested from host", "host files copied into guest");
  });

  await testStep("guest to host copies files with -a", async () => {
    await lnx(ctx, ["bash", "-lc", "mkdir -p /root/guest-src; printf guest-file >/root/guest-src/file.txt"]);
    await run([
      ctx.lnxBin,
      "--instance",
      ctx.instance,
      "cp",
      "-a",
      "/root/guest-src/file.txt",
      `host:${join(ctx.tmpdir, "host-out")}`,
    ]);
    assertEq(
      await readFile(join(ctx.tmpdir, "host-out", "root", "guest-src", "file.txt"), "utf8"),
      "guest-file",
      "guest file copied to host",
    );
  });

  await testStep("plain cp without host marker passes through to guest", async () => {
    await lnx(ctx, ["bash", "-lc", "printf passthrough >/root/plain-cp-source"]);
    await run([ctx.lnxBin, "--instance", ctx.instance, "cp", "/root/plain-cp-source", "/root/plain-cp-dest"]);
    assertEq((await lnx(ctx, ["cat", "/root/plain-cp-dest"])).stdout, "passthrough", "plain cp passthrough");
  });
} finally {
  await cleanupContext(ctx);
}
