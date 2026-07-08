import { randomUUID } from "node:crypto";
import { defineProvider, escapeShellArg } from "@computesdk/provider";
import type {
  CommandResult,
  FileEntry,
  RunCommandOptions,
  SandboxInfo,
} from "@computesdk/provider";
import {
  createLnxClient,
  LnxCommandError,
  type LnxClient,
  type LnxInstance,
} from "../../index";

const NAME_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;
const DEFAULT_TIMEOUT_MS = 300_000;
const DEFAULT_NAME_PREFIX = "csdk-";

export interface LnxProviderConfig {
  /** Path to (or name of) the lnx binary. Default: `process.env.LNX_BIN ?? "lnx"`. */
  binary?: string;
  /** Sandbox timeout reported in `SandboxInfo`, in milliseconds. Default: 300_000. */
  timeout?: number;
  /** Prefix used for auto-generated sandbox names. Default: "csdk-". */
  namePrefix?: string;
}

export interface LnxSandboxHandle {
  name: string;
  instance: LnxInstance;
  envs: Record<string, string>;
  createdAt?: Date;
  timeout: number;
  /**
   * ComputeSDK's `getUrl(sandbox, options)` sandbox method does not receive
   * the provider config, only the sandbox handle, but building a URL requires
   * checking whether ingress is enabled (`client.ingress.status()`). Stash
   * the client on the handle so getUrl can reach it without reconstructing
   * config from scratch.
   */
  client: LnxClient;
}

function resolveBinary(config: LnxProviderConfig): string {
  return config.binary ?? process.env.LNX_BIN ?? "lnx";
}

function resolveTimeout(config: LnxProviderConfig): number {
  return config.timeout ?? DEFAULT_TIMEOUT_MS;
}

function resolveNamePrefix(config: LnxProviderConfig): string {
  return config.namePrefix ?? DEFAULT_NAME_PREFIX;
}

function clientFor(config: LnxProviderConfig): LnxClient {
  return createLnxClient({ binary: resolveBinary(config) });
}

function toHandle(
  client: LnxClient,
  name: string,
  timeout: number,
  envs: Record<string, string> = {},
  createdAt?: Date,
): LnxSandboxHandle {
  return {
    name,
    instance: client.instance(name),
    envs,
    createdAt,
    timeout,
    client,
  };
}

async function readFile(
  sandbox: LnxSandboxHandle,
  path: string,
  runCommand: (
    sandbox: LnxSandboxHandle,
    command: string,
    options?: RunCommandOptions,
  ) => Promise<CommandResult>,
): Promise<string> {
  // `test -f` guards the pipeline: a pipeline's exit status is the LAST
  // command's (tr), which succeeds even when base64 fails on a missing file.
  const escapedPath = escapeShellArg(path);
  const result = await runCommand(
    sandbox,
    `test -f "${escapedPath}" && base64 "${escapedPath}" | tr -d '\\n'`,
  );
  if (result.exitCode !== 0) {
    throw new Error(`File not found: ${path}`);
  }
  return Buffer.from(result.stdout, "base64").toString("utf8");
}

async function writeFile(
  sandbox: LnxSandboxHandle,
  path: string,
  content: string,
  runCommand: (
    sandbox: LnxSandboxHandle,
    command: string,
    options?: RunCommandOptions,
  ) => Promise<CommandResult>,
): Promise<void> {
  const encoded = Buffer.from(content, "utf8").toString("base64");
  const escapedPath = escapeShellArg(path);
  await runCommand(
    sandbox,
    `mkdir -p "$(dirname "${escapedPath}")" && printf '%s' "${escapeShellArg(encoded)}" | base64 -d > "${escapedPath}"`,
  );
}

async function mkdir(
  sandbox: LnxSandboxHandle,
  path: string,
  runCommand: (
    sandbox: LnxSandboxHandle,
    command: string,
    options?: RunCommandOptions,
  ) => Promise<CommandResult>,
): Promise<void> {
  await runCommand(sandbox, `mkdir -p "${escapeShellArg(path)}"`);
}

async function exists(
  sandbox: LnxSandboxHandle,
  path: string,
  runCommand: (
    sandbox: LnxSandboxHandle,
    command: string,
    options?: RunCommandOptions,
  ) => Promise<CommandResult>,
): Promise<boolean> {
  const result = await runCommand(sandbox, `test -e "${escapeShellArg(path)}"`);
  return result.exitCode === 0;
}

async function remove(
  sandbox: LnxSandboxHandle,
  path: string,
  runCommand: (
    sandbox: LnxSandboxHandle,
    command: string,
    options?: RunCommandOptions,
  ) => Promise<CommandResult>,
): Promise<void> {
  await runCommand(sandbox, `rm -rf "${escapeShellArg(path)}"`);
}

async function readdir(
  sandbox: LnxSandboxHandle,
  path: string,
  runCommand: (
    sandbox: LnxSandboxHandle,
    command: string,
    options?: RunCommandOptions,
  ) => Promise<CommandResult>,
): Promise<FileEntry[]> {
  const result = await runCommand(sandbox, `ls -lA --time-style=+%s "${escapeShellArg(path)}"`);
  if (result.exitCode !== 0) {
    return [];
  }
  const entries: FileEntry[] = [];
  for (const line of result.stdout.split("\n")) {
    if (!line || line.startsWith("total ")) continue;
    const fields = line.split(/\s+/);
    const isDir = fields[0]?.[0] === "d";
    const size = Number(fields[4]);
    const modifiedEpochSeconds = Number(fields[5]);
    const name = fields.slice(6).join(" ");
    if (!name) continue;
    entries.push({
      name,
      type: isDir ? "directory" : "file",
      size: Number.isFinite(size) ? size : undefined,
      modified: Number.isFinite(modifiedEpochSeconds)
        ? new Date(modifiedEpochSeconds * 1000)
        : undefined,
    });
  }
  return entries;
}

export const lnx = defineProvider<LnxSandboxHandle, LnxProviderConfig>({
  name: "lnx",
  methods: {
    sandbox: {
      async create(config, options) {
        if (options?.templateId || options?.snapshotId) {
          throw new Error(
            "lnx provider does not support templateId/snapshotId yet; lnx checkpoints will back these in a future version.",
          );
        }
        const client = clientFor(config);
        const name = options?.name ?? `${resolveNamePrefix(config)}${randomUUID().slice(0, 8)}`;
        if (!NAME_PATTERN.test(name)) {
          throw new Error(`invalid lnx sandbox name: ${name}`);
        }
        const timeout = options?.timeout ?? resolveTimeout(config);
        const instance = client.instance(name);
        // Boots the VM; the first-ever run may download the kernel/rootfs images.
        await instance.run(["/bin/true"]);
        const sandbox: LnxSandboxHandle = {
          name,
          instance,
          envs: options?.envs ?? {},
          timeout,
          client,
        };
        return { sandbox, sandboxId: name };
      },

      async getById(config, sandboxId) {
        const client = clientFor(config);
        const rows = await client.instances.list();
        if (!rows.some((row) => row.name === sandboxId)) {
          return null;
        }
        const instance = client.instance(sandboxId);
        const inspect = await instance.inspect();
        const sandbox: LnxSandboxHandle = {
          name: sandboxId,
          instance,
          // Options passed to create() (e.g. envs) are not persisted by lnx,
          // so they cannot be recovered here.
          envs: {},
          createdAt: new Date(inspect.created),
          timeout: resolveTimeout(config),
          client,
        };
        return { sandbox, sandboxId };
      },

      async list(config) {
        const client = clientFor(config);
        const rows = await client.instances.list();
        return rows
          .filter((row) => row.state !== "partial")
          .map((row) => ({
            sandbox: toHandle(client, row.name, resolveTimeout(config)),
            sandboxId: row.name,
          }));
      },

      async destroy(config, sandboxId) {
        const client = clientFor(config);
        try {
          await client.instances.delete(sandboxId);
        } catch (error) {
          if (error instanceof LnxCommandError && error.result.stderr.includes("instance not found")) {
            return;
          }
          throw error;
        }
      },

      async runCommand(sandbox, command, options) {
        const merged = { ...sandbox.envs, ...options?.env };
        const effectiveCommand = options?.background
          ? `nohup ${command} >/dev/null 2>&1 &`
          : command;
        const argv =
          Object.keys(merged).length > 0
            ? [
                "/usr/bin/env",
                ...Object.entries(merged).map(([key, value]) => `${key}=${value}`),
                "/bin/sh",
                "-c",
                effectiveCommand,
              ]
            : ["/bin/sh", "-c", effectiveCommand];

        const start = Date.now();
        const result = await sandbox.instance.run(argv, {
          check: false,
          cwd: options?.cwd,
          timeoutMs: options?.timeout,
        });
        const durationMs = Date.now() - start;
        return {
          stdout: result.stdout,
          stderr: result.stderr,
          exitCode: result.exitCode,
          durationMs,
        };
      },

      async getInfo(sandbox): Promise<SandboxInfo> {
        const inspect = await sandbox.instance.inspect();
        const status: SandboxInfo["status"] =
          inspect.state === "running" || inspect.state === "starting"
            ? "running"
            : inspect.state === "stopped"
              ? "stopped"
              : "error";
        return {
          id: sandbox.name,
          provider: "lnx",
          status,
          createdAt: sandbox.createdAt ?? new Date(inspect.created),
          timeout: sandbox.timeout,
          metadata: { state: inspect.state, checkpoints: inspect.checkpoints },
        };
      },

      async getUrl(sandbox, { port, protocol }) {
        const status = await sandbox.client.ingress.status();
        if (!status.enabled) {
          throw new Error(
            "lnx ingress is not enabled; run 'sudo lnx ingress enable' to get per-instance URLs, or use --forward for manual port forwarding.",
          );
        }
        const rawDomain = typeof status.domain === "string" ? status.domain : "";
        const domain = rawDomain.replace(/^\./, "") || "lnx";
        return `${protocol ?? "https"}://p${port}-${sandbox.name}.${domain}`;
      },

      getInstance(sandbox) {
        return sandbox;
      },

      filesystem: {
        readFile,
        writeFile,
        mkdir,
        readdir,
        exists,
        remove,
      },
    },
  },
});
