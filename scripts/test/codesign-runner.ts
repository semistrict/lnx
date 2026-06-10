#!/usr/bin/env bun

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

if (process.platform === "darwin" && (await exists(binary))) {
  const signStatus = await run([
    "codesign",
    "--entitlements",
    entitlements,
    "--force",
    "-s",
    "-",
    binary,
  ]);
  if (signStatus !== 0) {
    console.error(`codesign failed for ${binary}`);
    process.exit(signStatus);
  }
}

process.exit(await run([binary, ...args]));
