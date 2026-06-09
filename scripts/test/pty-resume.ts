import { assertContains, cleanupContext, defaultContext, prepareContext, run, testStep } from "./lib";

const ctx = defaultContext("pty-resume");

try {
  await prepareContext(ctx);

  await testStep("pty command resumes after lnxctl snapshot-exit", async () => {
    const result = await run(
      [
        "python3",
        "-",
        ctx.lnxBin,
        ctx.instance,
      ],
      {
        stdin: String.raw`
import errno
import os
import pty
import select
import subprocess
import sys

lnx_bin, instance = sys.argv[1], sys.argv[2]
master, slave = pty.openpty()
env = os.environ.copy()
env["TERM"] = "xterm-256color"
proc = subprocess.Popen(
    [lnx_bin, "--instance", instance, "--no-snapshot-restore", "bash", "-lc", "echo BEFORE; lnxctl snapshot-exit; echo AFTER"],
    stdin=slave,
    stdout=slave,
    stderr=slave,
    close_fds=True,
    env=env,
)
os.close(slave)
output = bytearray()
while True:
    ready, _, _ = select.select([master], [], [], 180)
    if not ready:
        proc.kill()
        raise SystemExit("timeout waiting for pty resume")
    try:
        chunk = os.read(master, 4096)
    except OSError as exc:
        if exc.errno == errno.EIO:
            break
        raise
    if not chunk:
        break
    output.extend(chunk)
    if b"AFTER" in output and proc.poll() is not None:
        break
status = proc.wait(timeout=10)
os.close(master)
sys.stdout.write(output.decode(errors="replace").replace("\r\n", "\n"))
raise SystemExit(status)
`,
        timeoutMs: 240_000,
      },
    );
    assertContains(result.stdout, "BEFORE", "pty saw pre-snapshot output");
    assertContains(result.stdout, "AFTER", "pty saw post-resume output");
  });
} finally {
  await cleanupContext(ctx);
}
