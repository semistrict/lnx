#!/usr/bin/env bun

import { mkdirSync, readFileSync, rmdirSync, statSync, writeFileSync } from "node:fs";

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

// Test harnesses (cargo-nextest) invoke this runner several times for the
// same binary, concurrently. codesign --force rewrites the file in place, so
// concurrent signing corrupts or momentarily removes it. Serialize with a
// lock directory and skip re-signing an unchanged binary via a stamp file.
async function withSignLock<T>(path: string, fn: () => Promise<T>): Promise<T> {
  const lockDir = `${path}.sign-lock`;
  const start = Date.now();
  for (;;) {
    try {
      mkdirSync(lockDir);
      break;
    } catch {
      if (Date.now() - start > 30_000) {
        try {
          rmdirSync(lockDir);
        } catch {}
      }
      await Bun.sleep(25);
    }
  }
  try {
    return await fn();
  } finally {
    try {
      rmdirSync(lockDir);
    } catch {}
  }
}

function binaryStamp(path: string): string {
  const stat = statSync(path);
  return `${stat.size}:${stat.mtimeMs}`;
}

if (process.platform === "darwin" && (await exists(binary))) {
  const stampFile = `${binary}.signed`;
  const status = await withSignLock(binary, async () => {
    let previous = "";
    try {
      previous = readFileSync(stampFile, "utf8");
    } catch {}
    if (previous === binaryStamp(binary)) {
      return 0;
    }
    const signStatus = await run([
      "codesign",
      "--entitlements",
      entitlements,
      "--force",
      "-s",
      "-",
      binary,
    ]);
    if (signStatus === 0) {
      writeFileSync(stampFile, binaryStamp(binary));
    }
    return signStatus;
  });
  if (status !== 0) {
    console.error(`codesign failed for ${binary}`);
    process.exit(status);
  }
}

process.exit(await run([binary, ...args]));
