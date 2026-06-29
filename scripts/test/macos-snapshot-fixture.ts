import { existsSync } from "node:fs";
import { mkdir, readdir, readFile, rm } from "node:fs/promises";
import { dirname, join } from "node:path";
import {
  assertContains,
  assertEq,
  cloneSparseImage,
  cleanupContext,
  defaultContext,
  prepareContext,
  run,
  sleep,
  spawn,
} from "./lib";

if (process.platform !== "darwin") {
  throw new Error("macOS snapshot fixture creation must run on macOS");
}

const ctx = defaultContext("macos-snapshot-fixture");
const output =
  Bun.env.LNX_MACOS_SNAPSHOT_FIXTURE_OUT ??
  join(ctx.repoRoot, "target", "macos-linux-snapshot-fixture");
const checkpointName = Bun.env.LNX_MACOS_SNAPSHOT_CHECKPOINT_NAME ?? "macos-linux-fixture";
const fixtureKernel = Bun.env.LNX_MACOS_SNAPSHOT_KERNEL;
const kernelArgs = fixtureKernel ? ["--kernel", fixtureKernel] : [];

function collectOutput(stream: ReadableStream<Uint8Array>) {
  const decoder = new TextDecoder();
  let text = "";
  let done = false;
  const finished = (async () => {
    const reader = stream.getReader();
    while (true) {
      const chunk = await reader.read();
      if (chunk.done) {
        done = true;
        return text;
      }
      text += decoder.decode(chunk.value, { stream: true });
    }
  })();
  return {
    finished,
    async waitFor(needle: string, timeoutMs: number, label: string) {
      const deadline = Date.now() + timeoutMs;
      while (Date.now() < deadline) {
        if (text.includes(needle)) {
          return;
        }
        if (done) {
          break;
        }
        await sleep(100);
      }
      throw new Error(`timeout waiting for ${label}; saw:\n${text}`);
    },
  };
}

async function checkpointPathByName(imageDir: string, name: string): Promise<string> {
  const checkpointDir = join(imageDir, "checkpoints");
  for (const entry of await readdir(checkpointDir, { withFileTypes: true })) {
    if (!entry.isDirectory()) {
      continue;
    }
    const path = join(checkpointDir, entry.name);
    const meta = await readFile(join(path, "checkpoint.meta"), "utf8");
    if (meta.split("\n").includes(`name=${name}`)) {
      return path;
    }
  }
  throw new Error(`checkpoint not found: ${name}`);
}

try {
  await prepareContext(ctx);
  await rm(output, { recursive: true, force: true });
  await mkdir(dirname(output), { recursive: true });

  const source = spawn(
    [
      ctx.lnxBin,
      "--instance",
      ctx.instance,
      ...kernelArgs,
      "--no-host-shares",
      "--memory-mib",
      "512",
      "--cpus",
      "1",
      "python3",
      "-",
    ],
    {
      stdin: "pipe",
      env: {
        LNX_BROKER_IDLE_TTL_MS: "250",
        LNX_INGRESS_STATE_DIR: join(ctx.tmpdir, "disabled-ingress"),
      },
    },
  );
  const stdout = collectOutput(source.stdout);
  const stderr = collectOutput(source.stderr);
  await source.stdin.write(String.raw`
import subprocess
import time
from pathlib import Path

if "arm64.nopauth" not in Path("/proc/cmdline").read_text():
    raise SystemExit("portable snapshot source missing arm64.nopauth")

subprocess.run(["sudo", "tee", "/root/lnx-cross-host-disk"], input=b"macos-disk", stdout=subprocess.DEVNULL, check=True)
subprocess.run(["sudo", "tee", "/run/lnx-cross-host-memory"], input=b"macos-memory", stdout=subprocess.DEVNULL, check=True)
print("mac-source-ready", flush=True)

go = Path("/run/lnx-cross-host-go")
deadline = time.time() + 300
while time.time() < deadline and not go.exists():
    time.sleep(0.1)
if not go.exists():
    raise SystemExit("resume signal timed out")

subprocess.run(["sudo", "tee", "/root/lnx-cross-host-after"], input=b"macos-after", stdout=subprocess.DEVNULL, check=True)
print("mac-source-after", flush=True)
`);
  source.stdin.end();

  try {
    await stdout.waitFor("mac-source-ready", 180_000, "macOS source ready marker");
    const checkpoint = await run(
      [
        ctx.lnxBin,
        "--instance",
        ctx.instance,
        ...kernelArgs,
        "--no-host-shares",
        "checkpoint",
        "-m",
        checkpointName,
      ],
      {
        timeoutMs: 240_000,
        env: {
          LNX_INGRESS_STATE_DIR: join(ctx.tmpdir, "disabled-ingress"),
        },
      },
    );
    assertEq(checkpoint.stdout, checkpointName, "macOS live checkpoint label");
  } finally {
    const wake = await run(
      [
        ctx.lnxBin,
        "--instance",
        ctx.instance,
        ...kernelArgs,
        "--no-host-shares",
        "sudo",
        "sh",
        "-c",
        "printf go >/run/lnx-cross-host-go",
      ],
      {
        timeoutMs: 120_000,
        check: false,
        env: {
          LNX_INGRESS_STATE_DIR: join(ctx.tmpdir, "disabled-ingress"),
        },
      },
    ).catch(() => ({ status: 1 }));
    if (wake.status === 0) {
      await stdout.waitFor("mac-source-after", 120_000, "macOS source clean exit marker").catch(() => {});
    }
    const exited = await Promise.race([
      source.exited.then(() => true).catch(() => true),
      sleep(120_000).then(() => false),
    ]);
    if (!exited) {
      source.kill("SIGKILL");
    }
    await source.exited.catch(() => {});
    await stdout.finished.catch(() => "");
    await stderr.finished.catch(() => "");
  }

  const snapshot = await checkpointPathByName(ctx.imageDir, checkpointName);
  for (const file of ["vmstate.bin", "pages.img", "rootfs.ext4", "shares.stamp", "initramfs.stamp"]) {
    const path = join(snapshot, file);
    if (!existsSync(path)) {
      throw new Error(`missing macOS snapshot file: ${path}`);
    }
  }
  const sharesStamp = await Bun.file(join(snapshot, "shares.stamp")).text();
  assertContains(sharesStamp, "host-shares=disabled-v1", "source snapshot has host shares disabled");
  assertContains(sharesStamp, "net=gvproxy", "source snapshot uses portable gvproxy backing");

  await mkdir(output, { recursive: true });
  await cloneSparseImage(join(snapshot, "rootfs.ext4"), join(output, "rootfs.ext4"));
  await cloneSparseImage(join(snapshot, "pages.img"), join(output, "pages.img"));
  for (const file of ["vmstate.bin", "shares.stamp", "initramfs.stamp"]) {
    await run(["cp", join(snapshot, file), join(output, file)], { timeoutMs: 180_000 });
  }
  if (fixtureKernel) {
    await run(["cp", fixtureKernel, join(output, "vmlinuz")], { timeoutMs: 180_000 });
  }

  console.log(`LNX_MACOS_SNAPSHOT_FIXTURE=${output}`);
} finally {
  await cleanupContext(ctx);
}
