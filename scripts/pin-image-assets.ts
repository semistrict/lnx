// Pins the image release lnx downloads from: writes src/release_assets.json
// with the release tag and the SHA-256 GitHub reports for each asset lnx
// fetches, which the binary then embeds and checks every download against.
//
//   bun run images:pin images-v0.7.0

import { writeFile } from "node:fs/promises";
import { join } from "node:path";

const ASSETS = ["lnx-linux-aarch64", "rootfs.ext4.zst", "vmlinuz.gz"];
const release = process.argv[2];
if (!release?.startsWith("images-v")) {
  console.error("usage: bun run images:pin images-vX.Y.Z");
  process.exit(2);
}

const proc = Bun.spawn(["gh", "api", `repos/semistrict/lnx/releases/tags/${release}`], { stdout: "pipe", stderr: "inherit" });
const [body, status] = await Promise.all([new Response(proc.stdout).text(), proc.exited]);
if (status !== 0) process.exit(status);
const { assets } = JSON.parse(body) as { assets: Array<{ name: string; digest: string | null }> };

const sha256: Record<string, string> = {};
for (const name of ASSETS) {
  const digest = assets.find((asset) => asset.name === name)?.digest;
  const hex = digest?.match(/^sha256:([0-9a-f]{64})$/)?.[1];
  if (!hex) {
    console.error(`${release} has no SHA-256 digest for ${name} (got ${digest ?? "no such asset"})`);
    process.exit(1);
  }
  sha256[name] = hex;
}

const path = join(import.meta.dir, "..", "src", "release_assets.json");
await writeFile(path, JSON.stringify({ release, sha256 }, null, 2) + "\n");
console.log(`pinned ${release} in ${path}`);
