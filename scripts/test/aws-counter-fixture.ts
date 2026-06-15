import { existsSync } from "node:fs";
import { mkdir, readFile, rm, writeFile } from "node:fs/promises";
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
  throw new Error("AWS counter fixture creation currently runs from the local macOS host");
}

const ctx = defaultContext("aws-counter-fixture");
const output = Bun.env.LNX_AWS_COUNTER_FIXTURE_OUT ?? join(ctx.repoRoot, "target", "aws-counter-fixture-v8");
const checkpointName = Bun.env.LNX_AWS_COUNTER_CHECKPOINT_NAME ?? "aws-counter-v8";

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

const portableEnv = {
  LNX_BROKER_IDLE_TTL_MS: "30000",
  LNX_INGRESS_STATE_DIR: join(ctx.tmpdir, "disabled-ingress"),
  LNX_ROOTFS_BACKEND: "block",
};

try {
  await prepareContext(ctx);
  await rm(output, { recursive: true, force: true });
  await mkdir(dirname(output), { recursive: true });

  const source = spawn(
    [
      ctx.lnxBin,
      "--instance",
      ctx.instance,
      "--no-host-shares",
      "--memory-mib",
      "512",
      "--cpus",
      "1",
      "python3",
      "-",
    ],
    { stdin: "pipe", env: portableEnv },
  );
  const stdout = collectOutput(source.stdout);
  const stderr = collectOutput(source.stderr);
  await source.stdin.write(String.raw`
import subprocess

subprocess.run(["sudo", "sh", "-c", "printf 41 >/run/lnx-memory-counter"], check=True)
busy = subprocess.Popen(
    ["python3", "-c", r"""
marker = b"LNXAWSCOUNTERv8\0"
buf = bytearray(4096)
buf[:len(marker)] = marker
i = 41
buf[len(marker):len(marker) + 8] = i.to_bytes(8, "little")
while True:
    i += 1
    buf[len(marker):len(marker) + 8] = i.to_bytes(8, "little")
"""],
    stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
    start_new_session=True,
)
print(f"busy-pid={busy.pid}", flush=True)
print("counter-ready", flush=True)
subprocess.run(["lnxctl", "snapshot-exit"], check=True)
print("snapshot-exit-ready", flush=True)
`);
  source.stdin.end();

  try {
    await stdout.waitFor("busy-pid=", 180_000, "busy loop marker");
    await stdout.waitFor("counter-ready", 180_000, "counter ready marker");
    await stdout.waitFor("snapshot-exit-ready", 240_000, "snapshot-exit marker");
    const exited = await Promise.race([
      source.exited.then(() => true).catch(() => true),
      sleep(120_000).then(() => false),
    ]);
    if (!exited) {
      source.kill("SIGKILL");
      throw new Error("counter writer did not exit before checkpoint");
    }
    await source.exited;
  } finally {
    const exited = await Promise.race([
      source.exited.then(() => true).catch(() => true),
      sleep(1_000).then(() => false),
    ]);
    if (!exited) {
      source.kill("SIGKILL");
    }
    await source.exited.catch(() => {});
    await stdout.finished.catch(() => "");
    await stderr.finished.catch(() => "");
  }

  const snapshot = join(ctx.snapshotDir, "latest");
  for (const file of ["vmstate.bin", "pages.img", "rootfs.ext4", "shares.stamp", "initramfs.stamp"]) {
    const path = join(snapshot, file);
    if (!existsSync(path)) {
      throw new Error(`missing AWS counter snapshot file: ${path}`);
    }
  }

  const sharesStamp = await Bun.file(join(snapshot, "shares.stamp")).text();
  assertContains(sharesStamp, "host-shares=disabled-v1", "source snapshot has host shares disabled");
  assertContains(sharesStamp, "net=gvproxy", "source snapshot uses portable gvproxy backing");

  await mkdir(output, { recursive: true });
  await cloneSparseImage(join(snapshot, "rootfs.ext4"), join(output, "rootfs.ext4"));
  await cloneSparseImage(join(snapshot, "pages.img"), join(output, "pages.img"));
  for (const file of ["vmstate.bin", "shares.stamp", "initramfs.stamp", "checkpoint.meta"]) {
    if (file === "checkpoint.meta") {
      continue;
    }
    await run(["cp", join(snapshot, file), join(output, file)], { timeoutMs: 180_000 });
  }
  await writeFile(
    join(output, "checkpoint.meta"),
    `version=1\nid=snapshot-exit-${Date.now()}\nsource_instance=${ctx.instance}\ncreated_unix=${Math.floor(Date.now() / 1000)}\nname=${checkpointName}\n`,
  );

  console.log(`LNX_AWS_COUNTER_FIXTURE=${output}`);
} finally {
  await cleanupContext(ctx);
}

process.exit(0);
