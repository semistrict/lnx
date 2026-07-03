#!/usr/bin/env bun

import { mkdirSync, rmdirSync } from "node:fs";

const [binary, ...args] = Bun.argv.slice(2);
const entitlements = new URL("../../entitlements.plist", import.meta.url).pathname;

if (!binary) {
  console.error("usage: codesign-runner <test-binary> [args...]");
  process.exit(1);
}

async function exists(path: string): Promise<boolean> {
  try {
    const file = Bun.file(path);
    return await file.exists();
  } catch {
    return false;
  }
}

async function run(argv: string[]): Promise<number> {
  const proc = Bun.spawn(argv, {
    stdin: "inherit",
    stdout: "inherit",
    stderr: "inherit",
  });
  return await proc.exited;
}

async function hasHypervisorEntitlement(): Promise<boolean> {
  const proc = Bun.spawn(["codesign", "-d", "--entitlements", "-", binary], {
    stdin: "ignore",
    stdout: "pipe",
    stderr: "ignore",
  });
  const [status, stdout] = await Promise.all([
    proc.exited,
    new Response(proc.stdout).text(),
  ]);
  return status === 0 && stdout.includes("com.apple.security.hypervisor");
}

// Concurrent runners for the same binary (nextest spawns one process per
// test, plus parallel normal/ignored list passes) must not re-sign it while
// a sibling is signing or executing it: codesign --force replaces the file,
// and macOS kills running instances of a replaced binary. Serialize the
// signing behind a lock directory and skip it once the binary already
// carries the entitlements, so only the first invocation ever rewrites.
async function signOnce(): Promise<number> {
  const lockDir = `${binary}.codesign.lock`;
  const deadline = Date.now() + 120_000;
  for (;;) {
    try {
      mkdirSync(lockDir);
      break;
    } catch {
      if (Date.now() > deadline) {
        console.error(`timed out waiting for ${lockDir}; signing unlocked`);
        break;
      }
      await Bun.sleep(50);
    }
  }
  try {
    if (await hasHypervisorEntitlement()) {
      return 0;
    }
    return await run([
      "codesign",
      "--entitlements",
      entitlements,
      "--force",
      "-s",
      "-",
      binary,
    ]);
  } finally {
    try {
      rmdirSync(lockDir);
    } catch {}
  }
}

if (process.platform === "darwin" && (await exists(binary))) {
  const signStatus = await signOnce();
  if (signStatus !== 0) {
    console.error(`codesign failed for ${binary}`);
    process.exit(signStatus);
  }
}

process.exit(await run([binary, ...args]));
