import { existsSync } from "node:fs";
import { mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import {
  assertContains,
  assertEq,
  assertFile,
  cleanupContext,
  cleanupInstance,
  defaultContext,
  lnx,
  prepareContext,
  run,
  testStep,
  waitForVmSuspend,
} from "./lib";

const ctx = defaultContext("oci-import");
const image = "alpine:3.21";
const noInitInstance = `${ctx.instance}-noinit`;
const scratch = join(ctx.repoRoot, ".lnx-oci-test");

function debugfsTool(): string {
  for (const dir of ["/opt/homebrew/opt/e2fsprogs/sbin", "/usr/local/opt/e2fsprogs/sbin", "/usr/sbin", "/sbin"]) {
    if (existsSync(join(dir, "debugfs"))) {
      return join(dir, "debugfs");
    }
  }
  return "debugfs";
}

async function debugfsHas(image: string, path: string): Promise<boolean> {
  const result = await run([debugfsTool(), "-R", `stat ${path}`, image], { check: false });
  return !`${result.stdout}\n${result.stderr}`.includes("File not found");
}

try {
  await prepareContext(ctx);

  await testStep("import an OCI image as the instance rootfs", async () => {
    await run([ctx.lnxBin, "--instance", ctx.instance, "init", "--image", image], {
      timeoutMs: 600_000,
    });
    assertFile(join(ctx.imageDir, "rootfs.ext4"), "imported rootfs");
    const descriptor = JSON.parse(await readFile(join(ctx.imageDir, "lnx.json"), "utf8"));
    assertEq(descriptor.image, `oci:${image}`, "descriptor records image source");
  });

  await testStep("alpine boots busybox init with a supervised agent", async () => {
    const release = await lnx(ctx, ["--no-snapshot-restore", "cat", "/etc/alpine-release"], {
      timeoutMs: 180_000,
    });
    assertContains(release.stdout, "3.21.", "alpine release file");
    assertEq(
      (await lnx(ctx, ["sh", "-c", "tr -d '\\0' < /proc/1/cmdline"])).stdout,
      "/sbin/init",
      "busybox init owns pid 1",
    );
    assertEq((await lnx(ctx, ["id", "-un"])).stdout, "lnxuser", "exec user provisioned");
    const hostGroup = (await run(["id", "-gn"])).stdout;
    assertEq((await lnx(ctx, ["id", "-gn"])).stdout, hostGroup, "primary group named like host");
  });

  await testStep("login shell resolves to the image shell", async () => {
    assertEq(
      (await lnx(ctx, [], { stdin: "echo shell-ok; exit\n" })).stdout,
      "shell-ok",
      "default login shell over stdin",
    );
    assertEq(
      (await lnx(ctx, ["sh", "-c", 'grep "^lnxuser:" /etc/passwd | cut -d: -f7'])).stdout,
      "/bin/sh",
      "lnxuser shell is the image default",
    );
  });

  await testStep("snapshot restore works for the imported image", async () => {
    await lnx(ctx, ["sh", "-c", "echo oci-memory > /tmp/oci-marker"]);
    await waitForVmSuspend(ctx);
    assertEq(
      (await lnx(ctx, ["cat", "/tmp/oci-marker"])).stdout,
      "oci-memory",
      "restored guest keeps memory state",
    );
  });

  await testStep("layer whiteouts delete and mask files", async () => {
    // Stage inside the repo so the builder VM keeps the same share
    // topology as production imports (a cwd outside $HOME changes the
    // virtio device count and invalidates the builder's snapshot).
    const staging = join(scratch, "whiteout-staging");
    const lower = join(scratch, "layer-lower");
    const upper = join(scratch, "layer-upper");
    await mkdir(staging, { recursive: true });
    await mkdir(join(lower, "sub"), { recursive: true });
    await mkdir(join(upper, "sub"), { recursive: true });
    await writeFile(join(lower, "keep.txt"), "keep");
    await writeFile(join(lower, "gone.txt"), "gone");
    await writeFile(join(lower, "sub", "old.txt"), "old");
    await writeFile(join(upper, ".wh.gone.txt"), "");
    await writeFile(join(upper, "sub", ".wh..wh..opq"), "");
    await writeFile(join(upper, "sub", "new.txt"), "new");
    await writeFile(join(upper, "top.txt"), "top");
    await run(["tar", "-cf", join(staging, "layer-000"), "-C", lower, "."]);
    await run(["tar", "-cf", join(staging, "layer-001"), "-C", upper, "."]);

    await run([ctx.lnxBin, "_oci-build", staging], { timeoutMs: 300_000 });

    const image = join(staging, "rootfs.ext4");
    assertEq(await debugfsHas(image, "/keep.txt"), true, "lower file kept");
    assertEq(await debugfsHas(image, "/top.txt"), true, "upper file added");
    assertEq(await debugfsHas(image, "/sub/new.txt"), true, "opaque dir repopulated");
    assertEq(await debugfsHas(image, "/gone.txt"), false, "whiteout removed file");
    assertEq(await debugfsHas(image, "/sub/old.txt"), false, "opaque dir cleared");
    assertEq(await debugfsHas(image, "/.wh.gone.txt"), false, "whiteout marker not extracted");
  });

  await testStep("an init-less image keeps the agent as pid 1", async () => {
    await run(
      [ctx.lnxBin, "--instance", noInitInstance, "init", "--image", "debian:stable-slim"],
      { timeoutMs: 600_000 },
    );
    const probe = await run(
      [
        ctx.lnxBin,
        "--instance",
        noInitInstance,
        "--no-snapshot-restore",
        "cat",
        "/proc/1/comm",
      ],
      { timeoutMs: 180_000 },
    );
    assertEq(probe.stdout, "init", "shim stays pid 1 without an image init");
    const cmdline = await run(
      [ctx.lnxBin, "--instance", noInitInstance, "sh", "-c", "tr -d '\\0' < /proc/1/cmdline"],
    );
    // argv is ["/init", "--init"]; tr strips the NUL separators.
    assertEq(cmdline.stdout, "/init--init", "pid 1 is the boot shim, not an image init");
    const shell = await run(
      [ctx.lnxBin, "--instance", noInitInstance, "sh", "-c", 'grep "^lnxuser:" /etc/passwd | cut -d: -f7'],
    );
    // slim images ship no adduser.conf; /etc/default/useradd declares /bin/sh.
    assertEq(shell.stdout, "/bin/sh", "debian-slim useradd default shell");
  });
} finally {
  await cleanupContext(ctx);
  await cleanupInstance(ctx, noInitInstance);
  await rm(scratch, { recursive: true, force: true });
}
