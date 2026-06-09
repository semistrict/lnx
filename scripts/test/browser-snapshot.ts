import { createServer } from "node:net";
import { assertContains, assertEq, cleanupContext, defaultContext, lnx, prepareContext, run, skip, sleep, spawn, testStep } from "./lib";

const ctx = defaultContext("browser-snapshot");
const forkName = `${ctx.instance}-browser-fork`;

async function freePort(): Promise<number> {
  return await new Promise((resolve, reject) => {
    const server = createServer();
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (address && typeof address === "object") {
        server.close(() => resolve(address.port));
      } else {
        server.close(() => reject(new Error("could not allocate port")));
      }
    });
    server.on("error", reject);
  });
}

async function waitForCdp(port: number): Promise<string> {
  const listUrl = `http://127.0.0.1:${port}/json/list`;
  const newUrl = `http://127.0.0.1:${port}/json/new?https://example.com`;
  let lastError = "";
  for (let i = 0; i < 90; i++) {
    try {
      const response = await fetch(listUrl);
      if (response.ok) {
        const targets = await response.json() as Array<{ type?: string; webSocketDebuggerUrl?: string }>;
        const page = targets.find((target) => target.type === "page" && target.webSocketDebuggerUrl);
        if (page?.webSocketDebuggerUrl) return page.webSocketDebuggerUrl;
      }
      const created = await fetch(newUrl, { method: "PUT" });
      if (created.ok) {
        const target = await created.json() as { webSocketDebuggerUrl?: string };
        if (target.webSocketDebuggerUrl) return target.webSocketDebuggerUrl;
      }
    } catch (error) {
      lastError = error instanceof Error ? error.message : String(error);
    }
    await sleep(1000);
  }
  throw new Error(`timed out waiting for CDP page target on ${listUrl}: ${lastError}`);
}

async function interactWithBrowserOverCdp(port: number): Promise<void> {
  const socket = new WebSocket(await waitForCdp(port));
  const pending = new Map<number, { resolve: (value: unknown) => void; reject: (error: Error) => void }>();
  let nextId = 1;

  socket.addEventListener("message", (event) => {
    const message = JSON.parse(String(event.data)) as { id?: number; error?: { message?: string }; result?: unknown };
    if (message.id === undefined) return;
    const waiter = pending.get(message.id);
    if (!waiter) return;
    pending.delete(message.id);
    if (message.error) {
      waiter.reject(new Error(message.error.message ?? "CDP command failed"));
    } else {
      waiter.resolve(message.result);
    }
  });

  await new Promise<void>((resolve, reject) => {
    socket.addEventListener("open", () => resolve(), { once: true });
    socket.addEventListener("error", () => reject(new Error("CDP websocket failed to open")), { once: true });
  });

  const send = async (method: string, params: Record<string, unknown> = {}) => {
    const id = nextId++;
    const result = new Promise<unknown>((resolve, reject) => pending.set(id, { resolve, reject }));
    socket.send(JSON.stringify({ id, method, params }));
    return await result;
  };

  let closeWait: Promise<void> | undefined;
  try {
    await send("Page.enable");
    await send("Runtime.enable");
    await send("Page.navigate", { url: "https://example.com/#lnx-cdp-snapshot" });
    for (let i = 0; i < 90; i++) {
      const result = await send("Runtime.evaluate", {
        expression: "document.readyState + ':' + location.href",
        returnByValue: true,
      }) as { result?: { value?: string } };
      const value = result.result?.value ?? "";
      if (value.startsWith("complete:") && value.includes("lnx-cdp-snapshot")) break;
      await sleep(500);
    }
    await send("Runtime.evaluate", {
      expression: `
        document.body.insertAdjacentHTML('beforeend', '<main id="lnx-cdp-marker">lnx CDP snapshot marker</main>');
        window.__lnxSnapshotMarker = document.querySelector('#lnx-cdp-marker').textContent;
      `,
    });
    const marker = await send("Runtime.evaluate", {
      expression: "window.__lnxSnapshotMarker",
      returnByValue: true,
    }) as { result?: { value?: string } };
    assertEq(marker.result?.value, "lnx CDP snapshot marker", "CDP browser marker");
  } finally {
    closeWait = new Promise<void>((resolve) => {
      socket.addEventListener("close", () => resolve(), { once: true });
      setTimeout(() => resolve(), 1000);
    });
    socket.close();
    await closeWait;
  }
}

async function waitForProcessOutput(
  proc: ReturnType<typeof spawn>,
  needle: string,
  timeoutMs: number,
): Promise<void> {
  if (!proc.stdout) throw new Error("process stdout is not piped");
  const reader = proc.stdout.getReader();
  const decoder = new TextDecoder();
  let output = "";
  const deadline = Date.now() + timeoutMs;
  try {
    while (Date.now() < deadline) {
      const remaining = Math.max(1, deadline - Date.now());
      const next = await Promise.race([
        reader.read(),
        sleep(remaining).then(() => ({ done: false, value: undefined })),
      ]);
      if (next.value) {
        output += decoder.decode(next.value, { stream: true });
        if (output.includes(needle)) return;
      }
      if (next.done) break;
    }
  } finally {
    reader.releaseLock();
  }
  throw new Error(`timed out waiting for ${needle}; output:\n${output}`);
}

if (Bun.env.LNX_RUN_BROWSER_TEST !== "1") {
  await skip("browser snapshot test requires LNX_RUN_BROWSER_TEST=1 because it installs snap Chromium, a compositor, wayvnc, noVNC, and websockify");
}

try {
  await prepareContext(ctx);
  await run(["rm", "-rf", `${ctx.base}/images/${forkName}`, `${ctx.base}/instances/${forkName}`], { check: false });

  await testStep("install stock browser stack", async () => {
    await lnx(ctx, ["--no-snapshot-restore", "sudo", "apt-get", "update"], { timeoutMs: 300_000 });
    await lnx(ctx, ["bash", "-lc", "sudo DEBIAN_FRONTEND=noninteractive apt-get install -y snapd squashfs-tools cage wayvnc novnc websockify"], { timeoutMs: 600_000 });
    await lnx(ctx, ["sudo", "systemctl", "enable", "--now", "snapd.socket"], { timeoutMs: 120_000 });
    await lnx(ctx, ["bash", "-lc", "sudo systemctl start snapd.service || true"], { timeoutMs: 120_000 });
    await lnx(ctx, ["sudo", "snap", "install", "chromium"], { timeoutMs: 900_000 });
    const version = await lnx(ctx, ["/snap/bin/chromium", "--version"], { timeoutMs: 120_000 });
    assertContains(version.stdout, "Chromium", "snap chromium installed");
  });

  await testStep("start noVNC browser service and checkpoint", async () => {
    const cdpPort = await freePort();
    const owner = spawn([
      ctx.lnxBin,
      "--instance",
      ctx.instance,
      "--forward",
      `${cdpPort}:9222`,
      "bash",
      "-lc",
      String.raw`
sudo tee /usr/local/bin/lnx-browser-test >/dev/null <<'SH'
#!/bin/sh
set -eu
export XDG_RUNTIME_DIR=/run/user/0
export WLR_BACKENDS=headless
export WLR_LIBINPUT_NO_DEVICES=1
export WLR_RENDERER=pixman
export WAYLAND_DISPLAY=wayland-0
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR" || true
pkill wayvnc 2>/dev/null || true
pkill websockify 2>/dev/null || true
pkill cage 2>/dev/null || true
pkill chromium 2>/dev/null || true
rm -f "$XDG_RUNTIME_DIR"/wayland-*.lock "$XDG_RUNTIME_DIR"/wayland-[0-9] /tmp/lnx-cage.log /tmp/lnx-wayvnc.log /tmp/lnx-websockify.log
trap 'pkill wayvnc 2>/dev/null || true; pkill websockify 2>/dev/null || true; pkill cage 2>/dev/null || true' EXIT
cage -- /snap/bin/chromium --no-sandbox --disable-gpu --disable-dev-shm-usage --remote-debugging-address=127.0.0.1 --remote-debugging-port=9222 --user-data-dir=/tmp/lnx-browser-profile --ozone-platform=wayland --window-size=1280,800 https://example.com >/tmp/lnx-cage.log 2>&1 &
for i in $(seq 1 100); do
  [ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ] && break
  sleep 0.1
done
[ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ] || { tail -120 /tmp/lnx-cage.log; exit 1; }
printf 'enable_auth=false\n' >/tmp/lnx-wayvnc.ini
env XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_DISPLAY" wayvnc -C /tmp/lnx-wayvnc.ini --disable-input 127.0.0.1 5900 >/tmp/lnx-wayvnc.log 2>&1 &
for i in $(seq 1 100); do
  ss -ltn sport = :5900 | grep -q 5900 && break
  sleep 0.1
done
ss -ltn sport = :5900 | grep -q 5900 || { tail -120 /tmp/lnx-wayvnc.log; tail -120 /tmp/lnx-cage.log; exit 1; }
websockify --web /usr/share/novnc 127.0.0.1:6080 127.0.0.1:5900 >/tmp/lnx-websockify.log 2>&1 &
for i in $(seq 1 100); do
  ss -ltn sport = :6080 | grep -q 6080 && break
  sleep 0.1
done
ss -ltn sport = :6080 | grep -q 6080 || { tail -120 /tmp/lnx-websockify.log; exit 1; }
wait
SH
sudo chmod +x /usr/local/bin/lnx-browser-test
sudo tee /etc/systemd/system/lnx-browser-test.service >/dev/null <<'UNIT'
[Unit]
Description=lnx browser snapshot test
After=multi-user.target snapd.service

[Service]
Type=simple
ExecStart=/usr/local/bin/lnx-browser-test
Restart=on-failure

[Install]
WantedBy=multi-user.target
UNIT
sudo systemctl daemon-reload
sudo systemctl restart lnx-browser-test.service
for i in $(seq 1 30); do
  if curl -fsS http://127.0.0.1:6080/vnc.html | grep -i novnc >/dev/null && curl -fsS http://127.0.0.1:9222/json/version | grep -i webSocketDebuggerUrl >/dev/null; then
    echo LNX_BROWSER_READY
    sleep 120
    exit 0
  fi
  sleep 1
done
sudo systemctl status --no-pager lnx-browser-test.service || true
sudo journalctl -u lnx-browser-test.service --no-pager -n 80 || true
exit 1
`,
    ]);
    let checkpointCreated = false;
    try {
      await waitForProcessOutput(owner, "LNX_BROWSER_READY", 240_000);
      await interactWithBrowserOverCdp(cdpPort);
      assertEq((await run([ctx.lnxBin, "--instance", ctx.instance, "checkpoint", "-m", "browser-ready"], { timeoutMs: 240_000 })).stdout, "browser-ready", "browser checkpoint");
      checkpointCreated = true;
    } finally {
      owner.kill("SIGTERM");
      await owner.exited.catch(() => {});
    }
    assertEq(checkpointCreated, true, "browser checkpoint created");
  });

  await testStep("fork browser checkpoint and verify noVNC endpoint survives", async () => {
    assertEq((await run([ctx.lnxBin, "--instance", ctx.instance, "fork", "--checkpoint", "browser-ready", forkName], { timeoutMs: 240_000 })).stdout, forkName, "browser fork");
    const page = await run([ctx.lnxBin, "--instance", forkName, "curl", "-fsS", "http://127.0.0.1:6080/vnc.html"], { timeoutMs: 240_000 });
    assertContains(page.stdout.toLowerCase(), "novnc", "fork noVNC page");
  });
} finally {
  await cleanupContext(ctx);
  await run(["rm", "-rf", `${ctx.base}/images/${forkName}`, `${ctx.base}/instances/${forkName}`], { check: false });
}

process.exit(0);
