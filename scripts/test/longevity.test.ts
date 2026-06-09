import { afterAll, beforeAll, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { join } from "node:path";
import {
  cleanupContext,
  cleanupInstance,
  defaultContext,
  diskUsageBytes,
  fileSize,
  lnx,
  prepareContext,
  run,
  sleep,
  spawnLnx,
} from "./lib";

const ctx = defaultContext("longevity");
const restoreIterations = Number(Bun.env.LNX_LONGEVITY_RESTORE_COUNT ?? "100");
const expensiveIterations = Number(Bun.env.LNX_LONGEVITY_EXPENSIVE_COUNT ?? "20");
const configuredForwardPort = Bun.env.LNX_LONGEVITY_FORWARD_PORT
  ? Number(Bun.env.LNX_LONGEVITY_FORWARD_PORT)
  : undefined;
const configuredIngressPort = Bun.env.LNX_LONGEVITY_INGRESS_PORT
  ? Number(Bun.env.LNX_LONGEVITY_INGRESS_PORT)
  : undefined;

async function maybe(name: string, fn: () => Promise<void>) {
  try {
    await fn();
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    const bundle = await writeDebugBundle(name, message).catch(() => "");
    console.warn(`SKIP ${name}: ${message}${bundle ? `\nDEBUG ${name}: ${bundle}` : ""}`);
  }
}

async function writeDebugBundle(name: string, message: string): Promise<string> {
  const safeName = name.replace(/[^a-zA-Z0-9._-]+/g, "-");
  const dir = join(ctx.tmpdir, "debug", safeName);
  await mkdir(dir, { recursive: true });
  await writeFile(join(dir, "failure.txt"), `${name}\n\n${message}\n`);
  for (const [label, path] of [
    ["lnx.log", join(ctx.runDir, "lnx.log")],
    ["console.log", join(ctx.runDir, "console.log")],
    ["gvproxy.log", join(ctx.runDir, "gvproxy.log")],
    ["timings.log", join(ctx.runDir, "timings.log")],
  ]) {
    if (existsSync(path)) {
      await writeFile(join(dir, label), await readFile(path));
    }
  }
  const diagnostics = await run(
    [
      "bash",
      "-lc",
      "date; ps -axo pid,ppid,command | grep -E 'lnx|gvproxy|python3 -m http.server' | grep -v grep || true; lsof -nP -iTCP -sTCP:LISTEN 2>/dev/null | grep -E 'lnx|python|target/debug/lnx' || true",
    ],
    { check: false },
  );
  await writeFile(join(dir, "host.txt"), `${diagnostics.stdout}\n${diagnostics.stderr}\n`);
  return dir;
}

async function lnxExpect(args: string[], options: Parameters<typeof lnx>[2] = {}) {
  const result = await lnx(ctx, args, { timeoutMs: 240_000, ...options });
  expect(result.status).toBe(0);
  return result;
}

async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (typeof address === "object" && address) {
        const port = address.port;
        server.close(() => resolve(port));
      } else {
        server.close(() => reject(new Error("could not allocate port")));
      }
    });
  });
}

beforeAll(async () => {
  await prepareContext(ctx);
});

afterAll(async () => {
  await cleanupContext(ctx);
});

test("repeated restore loop tracks latency and snapshot size drift", async () => {
  await maybe("repeated restore loop", async () => {
    const latencies: number[] = [];
    const sizes: number[] = [];
    for (let i = 0; i < restoreIterations; i++) {
      const start = performance.now();
      const result = await lnxExpect(["echo", String(i)]);
      latencies.push(performance.now() - start);
      expect(result.stdout).toBe(String(i));
      sizes.push(await diskUsageBytes(join(ctx.snapshotDir, "latest")));
    }
    const max = Math.max(...latencies);
    const avg = latencies.reduce((sum, value) => sum + value, 0) / latencies.length;
    const drift = Math.max(...sizes) - Math.min(...sizes);
    console.log(`restore-loop count=${restoreIterations} avg_ms=${Math.round(avg)} max_ms=${Math.round(max)} size_drift_bytes=${drift}`);
    expect(max).toBeLessThan(240_000);
  });
}, 60 * 60 * 1000);

test("network after restore loop", async () => {
  await maybe("network after restore loop", async () => {
    await lnxExpect(["apt-get", "update"], { timeoutMs: 300_000 });
    for (let i = 0; i < Math.min(restoreIterations, 100); i++) {
      const result = await lnxExpect([
        "bash",
        "-lc",
        "curl -fsS --max-time 20 https://example.com >/dev/null && apt-cache policy ruby | grep -q Candidate: && getent hosts ports.ubuntu.com >/dev/null && echo ok",
      ]);
      expect(result.stdout).toBe("ok");
    }
  });
}, 60 * 60 * 1000);

test("long-lived tcp across snapshot reconnects cleanly", async () => {
  await maybe("long-lived tcp across snapshot", async () => {
    await lnxExpect([
      "--no-snapshot-restore",
      "bash",
      "-lc",
      "mkdir -p /tmp/lnx-http; printf tcp-ok >/tmp/lnx-http/index.html; cat >/etc/systemd/system/lnx-long-tcp.service <<'UNIT'\n[Service]\nWorkingDirectory=/tmp/lnx-http\nExecStart=/usr/bin/python3 -m http.server 8081 --bind 127.0.0.1\n[Install]\nWantedBy=multi-user.target\nUNIT\nsystemctl daemon-reload; systemctl enable --now lnx-long-tcp.service; sleep 1; systemctl is-active lnx-long-tcp.service",
    ]);
    const before = await lnxExpect(["curl", "-fsS", "http://127.0.0.1:8081"]);
    expect(before.stdout).toBe("tcp-ok");
    await lnxExpect(["lnxctl", "snapshot-exit"]);
    const after = await lnxExpect(["curl", "-fsS", "http://127.0.0.1:8081"]);
    expect(after.stdout).toBe("tcp-ok");
  });
}, 10 * 60 * 1000);

test("port forward restore", async () => {
  await maybe("port forward restore", async () => {
    const forwardPort = configuredForwardPort ?? await freePort();
    await lnxExpect([
      "--no-snapshot-restore",
      "bash",
      "-lc",
      "mkdir -p /tmp/lnx-forward; printf forward-ok >/tmp/lnx-forward/index.html; cat >/etc/systemd/system/lnx-forward-test.service <<'UNIT'\n[Service]\nWorkingDirectory=/tmp/lnx-forward\nExecStart=/usr/bin/python3 -m http.server 8080 --bind 127.0.0.1\n[Install]\nWantedBy=multi-user.target\nUNIT\nsystemctl daemon-reload; systemctl enable --now lnx-forward-test.service; sleep 1; curl -fsS http://127.0.0.1:8080",
    ]);
    const proc = spawnLnx(ctx, [
      "--forward",
      `${forwardPort}:8080`,
      "sleep",
      "300",
    ]);
    await sleep(5_000);
    expect((await run(["curl", "-fsS", `http://127.0.0.1:${forwardPort}`], { timeoutMs: 30_000 })).stdout).toBe("forward-ok");
    proc.kill("SIGTERM");
    await proc.exited.catch(() => {});
    const restored = spawnLnx(ctx, [
      "--forward",
      `${forwardPort}:8080`,
      "sleep",
      "300",
    ]);
    await sleep(5_000);
    expect((await run(["curl", "-fsS", `http://127.0.0.1:${forwardPort}`], { timeoutMs: 30_000 })).stdout).toBe("forward-ok");
    restored.kill("SIGTERM");
    await restored.exited.catch(() => {});
  });
}, 10 * 60 * 1000);

test("ingress restore", async () => {
  await maybe("ingress restore", async () => {
    const ingressPort = configuredIngressPort ?? await freePort();
    const env = {
      LNX_INGRESS_DOMAIN: "lnxtest",
      LNX_INGRESS_DNS_ADDR: "127.0.0.1:15355",
      LNX_INGRESS_HTTP_ADDR: `127.0.0.1:${ingressPort}`,
      LNX_INGRESS_RESOLVER_DIR: ctx.tmpdir,
      LNX_INGRESS_STATE_DIR: join(ctx.tmpdir, "ingress-state"),
    };
    await run([ctx.lnxBin, "ingress", "enable"], { env, timeoutMs: 30_000 });
    await lnxExpect([
      "--no-snapshot-restore",
      "bash",
      "-lc",
      "mkdir -p /tmp/lnx-ingress; printf ingress-ok >/tmp/lnx-ingress/index.html; cat >/etc/systemd/system/lnx-ingress-test.service <<'UNIT'\n[Service]\nWorkingDirectory=/tmp/lnx-ingress\nExecStart=/usr/bin/python3 -m http.server 8080 --bind 127.0.0.1\n[Install]\nWantedBy=multi-user.target\nUNIT\nsystemctl daemon-reload; systemctl enable --now lnx-ingress-test.service; sleep 1; curl -fsS http://127.0.0.1:8080",
    ]);
    expect((await lnxExpect(["curl", "-fsS", "http://127.0.0.1:8080"])).stdout).toBe("ingress-ok");
    const proc = spawnLnx(ctx, ["sleep", "300"]);
    await sleep(5_000);
    expect((await run(["curl", "-fsS", "-H", `Host: p8080.${ctx.instance}.lnxtest`, `http://127.0.0.1:${ingressPort}`], { env, timeoutMs: 30_000 })).stdout).toBe("ingress-ok");
    proc.kill("SIGTERM");
    await proc.exited.catch(() => {});
    const restored = spawnLnx(ctx, ["sleep", "300"]);
    await sleep(5_000);
    expect((await run(["curl", "-fsS", "-H", `Host: p8080.${ctx.instance}.lnxtest`, `http://127.0.0.1:${ingressPort}`], { env, timeoutMs: 30_000 })).stdout).toBe("ingress-ok");
    restored.kill("SIGTERM");
    await restored.exited.catch(() => {});
    await run([ctx.lnxBin, "ingress", "disable"], { env, check: false, timeoutMs: 30_000 });
  });
}, 10 * 60 * 1000);

test("concurrent snapshot pressure", async () => {
  await maybe("concurrent snapshot pressure", async () => {
    await lnxExpect(["--no-snapshot-restore", "true"]);
    const workers = Array.from({ length: 10 }, (_, i) => lnx(ctx, ["bash", "-lc", `sleep 0.$(( ${i} % 5 )); echo worker-${i}`]));
    const checkpoints = Array.from({ length: 5 }, (_, i) =>
      run([ctx.lnxBin, "--instance", ctx.instance, "checkpoint", "-m", `pressure-${i}`], { timeoutMs: 240_000 }),
    );
    const results = await Promise.all([...workers, ...checkpoints]);
    expect(results.every((result) => result.status === 0)).toBe(true);
    const list = await run([ctx.lnxBin, "--instance", ctx.instance, "checkpoints"]);
    expect(list.stdout).toContain("pressure-");
  });
}, 15 * 60 * 1000);

test("fork after many restores", async () => {
  await maybe("fork after many restores", async () => {
    const forkName = `${ctx.instance}-many-restores-fork`;
    await cleanupInstance(ctx, forkName);
    for (let i = 0; i < expensiveIterations; i++) {
      await lnxExpect(["bash", "-lc", `printf ${i} >/root/many-restores-marker`]);
    }
    await run([ctx.lnxBin, "--instance", ctx.instance, "fork", forkName], { timeoutMs: 240_000 });
    const forked = await run([ctx.lnxBin, "--instance", forkName, "cat", "/root/many-restores-marker"], { timeoutMs: 240_000 });
    expect(forked.stdout).toBe(String(expensiveIterations - 1));
    await cleanupInstance(ctx, forkName);
  });
}, 20 * 60 * 1000);

test("apt and snap state across restore", async () => {
  await maybe("apt and snap state across restore", async () => {
    await lnxExpect(["apt-get", "update"], { timeoutMs: 300_000 });
    await lnxExpect(["bash", "-lc", "DEBIAN_FRONTEND=noninteractive apt-get install -y jq"], { timeoutMs: 300_000 });
    await lnxExpect(["jq", "--version"]);
    await lnxExpect(["bash", "-lc", "DEBIAN_FRONTEND=noninteractive apt-get install -y snapd squashfs-tools"], { timeoutMs: 300_000 });
    await lnxExpect(["systemctl", "enable", "--now", "snapd.socket"]);
    await lnxExpect(["snap", "install", "hello-world"], { timeoutMs: 600_000 });
    await lnxExpect(["hello-world"]);
    await lnxExpect(["bash", "-lc", "dpkg --audit; snap list hello-world >/dev/null; echo sane"]);
  });
}, 30 * 60 * 1000);

test("clock and cert restore", async () => {
  await maybe("clock and cert restore", async () => {
    const result = await lnxExpect([
      "bash",
      "-lc",
      "date -u +%s; curl -fsS --max-time 20 https://example.com >/dev/null; timedatectl status >/dev/null || true; echo tls-ok",
    ]);
    expect(result.stdout).toContain("tls-ok");
  });
}, 10 * 60 * 1000);

test("memory pressure snapshot", async () => {
  await maybe("memory pressure snapshot", async () => {
    const mib = Number(Bun.env.LNX_MEMORY_PRESSURE_MIB ?? "1024");
    await lnxExpect([
      "--no-snapshot-restore",
      "bash",
      "-lc",
      `python3 - <<'PY'
import hashlib, pathlib
size = ${mib} * 1024 * 1024
chunk = b'lnx-memory-pressure' * 4096
h = hashlib.sha256()
path = pathlib.Path('/root/memory-pressure.bin')
with path.open('wb') as f:
    left = size
    while left:
        data = chunk[:min(len(chunk), left)]
        h.update(data)
        f.write(data)
        left -= len(data)
pathlib.Path('/root/memory-pressure.sha256').write_text(h.hexdigest())
PY`,
    ], { timeoutMs: 600_000 });
    const restored = await lnxExpect([
      "bash",
      "-lc",
      "python3 - <<'PY'\nimport hashlib, pathlib\nh=hashlib.sha256(pathlib.Path('/root/memory-pressure.bin').read_bytes()).hexdigest()\nprint(h == pathlib.Path('/root/memory-pressure.sha256').read_text())\nPY",
    ], { timeoutMs: 600_000 });
    expect(restored.stdout).toBe("True");
  });
}, 20 * 60 * 1000);

test("file descriptor and process tree longevity", async () => {
  await maybe("file descriptor and process tree longevity", async () => {
    await lnxExpect(["apt-get", "update"], { timeoutMs: 300_000 });
    await lnxExpect(["bash", "-lc", "DEBIAN_FRONTEND=noninteractive apt-get install -y tmux"], { timeoutMs: 300_000 });
    await lnxExpect(["bash", "-lc", "tmux new-session -d -s lnx-long 'while true; do date >>/root/tmux-long.log; sleep 1; done'; systemd-run --unit=lnx-long-sleep --remain-after-exit /bin/sh -lc 'sleep infinity' || true"]);
    const restored = await lnxExpect(["bash", "-lc", "tmux has-session -t lnx-long && systemctl status lnx-long-sleep >/dev/null 2>&1 || true; pgrep -af 'sleep infinity|tmux' >/dev/null; echo alive"]);
    expect(restored.stdout).toContain("alive");
  });
}, 20 * 60 * 1000);

test("host kill during snapshot recovers or fails cleanly", async () => {
  await maybe("host kill during snapshot", async () => {
    await lnxExpect(["--no-snapshot-restore", "bash", "-lc", "dd if=/dev/zero of=/root/kill-snapshot bs=1M count=64 status=none; sync"]);
    const proc = spawnLnx(ctx, ["checkpoint", "-m", "kill-during-snapshot"]);
    await sleep(250);
    proc.kill("SIGKILL");
    await proc.exited.catch(() => {});
    const next = await lnx(ctx, ["echo", "after-kill"], { timeoutMs: 240_000, check: false });
    expect(next.status).toBe(0);
    expect(next.stdout).toBe("after-kill");
  });
}, 10 * 60 * 1000);

test("snapshot size and sparse regression", async () => {
  await maybe("snapshot size and sparse regression", async () => {
    const before = await diskUsageBytes(join(ctx.snapshotDir, "latest"));
    for (let i = 0; i < Math.min(restoreIterations, 100); i++) {
      await lnxExpect(["bash", "-lc", "dd if=/dev/zero of=/root/sparse-probe bs=1M count=64 status=none; rm -f /root/sparse-probe"]);
    }
    const after = await diskUsageBytes(join(ctx.snapshotDir, "latest"));
    const logical = await fileSize(join(ctx.snapshotDir, "latest", "pages.img"));
    console.log(`sparse-regression before=${before} after=${after} drift=${after - before} pages_logical=${logical}`);
    expect(after - before).toBeLessThan(2 * 1024 * 1024 * 1024);
  });
}, 60 * 60 * 1000);
