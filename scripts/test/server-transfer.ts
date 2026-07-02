import {
  existsSync } from "node:fs";
import { link,
  mkdir,
  readFile,
  rm,
  writeFile } from "node:fs/promises";
import { join } from "node:path";
import { homedir,
  tmpdir } from "node:os";
import { assertEq,
  repoRoot,
  run,
  sleep,
  spawn,
  testStep,
} from "./lib";

const root = repoRoot();
const lnxBin = Bun.env.LNX_BIN ?? join(root, "target/debug/lnx");
const base = join(tmpdir(), `lnx-server-transfer-${process.pid}`);
const sourceBase = join(base, "source");
const destBase = join(base, "dest");
const sourceInstance = "server-source";
const targetInstance = "server-target";
const port = await freePort();
const url = `http://127.0.0.1:${port}`;
const launchMetadata = JSON.stringify({
  version: 1,
  owner_args: [],
  compatibility: {
    host_share_cache: "host-share-cache=nodax+close-to-open+writeback+restore-sync-v2",
    packages: "packages=disabled-v1",
    net: "net=gvproxy",
  },
  shares: {
    no_host_shares: false,
    host_home: homedir(),
    outside_home_cwd: null,
  },
}, null, 2) + "\n";

try {
  await rm(base, { recursive: true, force: true });
  await mkdir(join(sourceBase, "instances", sourceInstance, "memory-snapshots", "latest"), { recursive: true });
  await writeFile(join(sourceBase, "vmlinuz"), "kernel");
  await writeFile(join(sourceBase, "instances", sourceInstance, "rootfs.ext4"), "rootfs");
  await writeFile(join(sourceBase, "instances", sourceInstance, "vm-initialized"), "1\n");
  await writeFile(join(sourceBase, "instances", sourceInstance, "lnx.json"), JSON.stringify({ name: sourceInstance }) + "\n");
  await link(
    join(sourceBase, "instances", sourceInstance, "rootfs.ext4"),
    join(sourceBase, "instances", sourceInstance, "memory-snapshots", "latest", "rootfs.ext4"),
  );
  await writeFile(join(sourceBase, "instances", sourceInstance, "memory-snapshots", "latest", "pages.img"), "pages");
  await writeFile(join(sourceBase, "instances", sourceInstance, "memory-snapshots", "latest", "vmstate.bin"), "vmstate");
  await writeFile(join(sourceBase, "instances", sourceInstance, "memory-snapshots", "latest", "launch.json"), launchMetadata);
  await writeFile(join(sourceBase, "instances", sourceInstance, "memory-snapshots", "latest", "initramfs.stamp"), "stamp");

  const server = spawn([lnxBin, "server", "--listen", `127.0.0.1:${port}`], {
    env: { LNX_BASE: destBase },
    stdout: "pipe",
    stderr: "pipe",
  });
  try {
    await waitForServer(url);

    await testStep("push sandbox bundle", async () => {
      await run(
        [
          lnxBin,
          "--instance",
          sourceInstance,
          "server",
          "push",
          url,
          "--target-instance",
          targetInstance,
        ],
        { env: { LNX_BASE: sourceBase }, timeoutMs: 60_000 },
      );
    });

    await testStep("imported sandbox is usable on destination", async () => {
      assertEq(await readFile(join(destBase, "vmlinuz"), "utf8"), "kernel", "kernel import");
      assertEq(await readFile(join(destBase, "instances", targetInstance, "rootfs.ext4"), "utf8"), "rootfs", "rootfs import");
      assertEq(
        await readFile(join(destBase, "instances", targetInstance, "memory-snapshots", "latest", "vmstate.bin"), "utf8"),
        "vmstate",
        "snapshot import",
      );
      assertEq(existsSync(join(destBase, "instances", targetInstance, "vm-initialized")), true, "vm-initialized import");
      const descriptor = JSON.parse(await readFile(join(destBase, "instances", targetInstance, "lnx.json"), "utf8"));
      assertEq(descriptor.name, targetInstance, "descriptor renamed");
    });
  } finally {
    server.kill("SIGTERM");
    await Promise.race([server.exited.catch(() => {}), sleep(2_000)]);
    server.kill("SIGKILL");
    await server.exited.catch(() => {});
  }
} finally {
  await rm(base, { recursive: true, force: true });
}

async function waitForServer(baseUrl: string): Promise<void> {
  const deadline = Date.now() + 10_000;
  let last = "";
  while (Date.now() < deadline) {
    const result = await run(["curl", "-fsS", "--max-time", "2", `${baseUrl}/v1/health`], { check: false });
    if (result.status === 0) {
      return;
    }
    last = result.stderr || result.stdout;
    await sleep(100);
  }
  throw new Error(`timed out waiting for lnx server: ${last}`);
}

async function freePort(): Promise<number> {
  const proc = Bun.spawn(["python3", "-c", "import socket; s=socket.socket(); s.bind(('127.0.0.1', 0)); print(s.getsockname()[1]); s.close()"], {
    stdout: "pipe",
    stderr: "pipe",
  });
  const [status, stdout, stderr] = await Promise.all([
    proc.exited,
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
  ]);
  if (status !== 0) {
    throw new Error(stderr);
  }
  return Number(stdout.trim());
}
