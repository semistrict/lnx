import { assertContains, assertEq, cleanupContext, defaultContext, lnx, prepareContext, run, skip, testStep } from "./lib";

const ctx = defaultContext("browser-snapshot");
const forkName = `${ctx.instance}-browser-fork`;

if (Bun.env.LNX_RUN_BROWSER_TEST !== "1") {
  await skip("browser snapshot test requires LNX_RUN_BROWSER_TEST=1 because it installs snap Chromium, a compositor, wayvnc, noVNC, and websockify");
}

try {
  await prepareContext(ctx);
  await run(["rm", "-rf", `${ctx.base}/images/${forkName}`, `${ctx.base}/instances/${forkName}`], { check: false });

  await testStep("install stock browser stack", async () => {
    await lnx(ctx, ["--no-snapshot-restore", "apt-get", "update"], { timeoutMs: 300_000 });
    await lnx(ctx, ["bash", "-lc", "DEBIAN_FRONTEND=noninteractive apt-get install -y snapd squashfs-tools cage wayvnc novnc websockify"], { timeoutMs: 600_000 });
    await lnx(ctx, ["systemctl", "enable", "--now", "snapd.socket"], { timeoutMs: 120_000 });
    await lnx(ctx, ["bash", "-lc", "systemctl start snapd.service || true"], { timeoutMs: 120_000 });
    await lnx(ctx, ["snap", "install", "chromium"], { timeoutMs: 900_000 });
    const version = await lnx(ctx, ["/snap/bin/chromium", "--version"], { timeoutMs: 120_000 });
    assertContains(version.stdout, "Chromium", "snap chromium installed");
  });

  await testStep("start noVNC browser service and checkpoint", async () => {
    await lnx(ctx, [
      "bash",
      "-lc",
      String.raw`
cat >/usr/local/bin/lnx-browser-test <<'SH'
#!/bin/sh
set -eu
export XDG_RUNTIME_DIR=/run/user/0
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
pkill wayvnc 2>/dev/null || true
pkill websockify 2>/dev/null || true
pkill cage 2>/dev/null || true
trap 'pkill wayvnc 2>/dev/null || true; pkill websockify 2>/dev/null || true; pkill cage 2>/dev/null || true' EXIT
cage -- /snap/bin/chromium --no-sandbox --disable-gpu --disable-dev-shm-usage --user-data-dir=/tmp/lnx-browser-profile --ozone-platform=wayland --window-size=1280,800 https://example.com &
sleep 5
wayvnc --render-cursor 127.0.0.1 5900 &
websockify --web /usr/share/novnc 127.0.0.1:6080 127.0.0.1:5900 &
wait
SH
chmod +x /usr/local/bin/lnx-browser-test
cat >/etc/systemd/system/lnx-browser-test.service <<'UNIT'
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
systemctl daemon-reload
systemctl restart lnx-browser-test.service
for i in $(seq 1 30); do
  if curl -fsS http://127.0.0.1:6080/vnc.html | grep -i novnc; then
    exit 0
  fi
  sleep 1
done
systemctl status --no-pager lnx-browser-test.service || true
journalctl -u lnx-browser-test.service --no-pager -n 80 || true
exit 1
`,
    ], { timeoutMs: 240_000 });
    assertEq((await run([ctx.lnxBin, "--instance", ctx.instance, "checkpoint", "-m", "browser-ready"], { timeoutMs: 240_000 })).stdout, "browser-ready", "browser checkpoint");
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
