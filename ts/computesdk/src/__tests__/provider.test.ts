import "./dirname-shim";
import { spawnSync } from "node:child_process";
import { describe, expect, it } from "vitest";
import { runProviderTestSuite } from "@computesdk/test-utils";
import { createLnxClient } from "../../../index";
import { lnx } from "../index";

const binary = process.env.LNX_BIN ?? "lnx";

const available = spawnSync(binary, ["--help"]).status === 0;

let ingressEnabled = false;
if (available) {
  try {
    const status = await createLnxClient({ binary }).ingress.status();
    ingressEnabled = status.enabled;
  } catch {
    ingressEnabled = false;
  }
}

describe("lnx computesdk provider", () => {
  runProviderTestSuite({
    name: "lnx",
    provider: lnx({ binary }),
    supportsFilesystem: true,
    supportsGetUrl: ingressEnabled,
    skipIntegration: !available,
    timeout: 120_000,
  });
});

describe.skipIf(!available)("lnx provider lifecycle", () => {
  it("creates, lists, runs, and destroys a sandbox", async () => {
    const provider = lnx({ binary });
    const sandbox = await provider.sandbox.create();

    expect(sandbox.sandboxId.startsWith("csdk-")).toBe(true);

    const list = await provider.sandbox.list();
    expect(list.some((entry) => entry.sandboxId === sandbox.sandboxId)).toBe(true);

    const found = await provider.sandbox.getById(sandbox.sandboxId);
    expect(found).not.toBeNull();

    const result = await sandbox.runCommand("echo hi");
    expect(result.exitCode).toBe(0);
    expect(result.stdout.trim()).toBe("hi");

    await sandbox.destroy();

    const afterDestroy = await provider.sandbox.getById(sandbox.sandboxId);
    expect(afterDestroy).toBeNull();

    await expect(sandbox.destroy()).resolves.toBeUndefined();
  }, 120_000);
});
