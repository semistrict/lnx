import { spawn as spawnChild, type ChildProcess } from "node:child_process";
import { once } from "node:events";
import { join } from "node:path";
import { Readable, Writable } from "node:stream";

export type Environment = Record<string, string | number | boolean | undefined>;

export type CommandResult = {
  status: number;
  exitCode: number;
  stdout: string;
  stderr: string;
  command: string[];
  instance?: string;
};

export type CommandOptions = {
  cwd?: string;
  env?: Environment;
  stdin?: string | Uint8Array;
  timeoutMs?: number;
  check?: boolean;
  signal?: AbortSignal;
};

export type LnxClientOptions = {
  binary?: string;
  defaultInstance?: string;
  cwd?: string;
  env?: Environment;
};

export type RunOptions = Omit<CommandOptions, "cwd"> & {
  cwd?: string;
  processCwd?: string;
  kernel?: string;
  rootfs?: string;
  cpus?: number;
  memoryMiB?: number;
  memoryMib?: number;
  snapshot?: string;
  nestedKvm?: boolean;
  deterministic?: string | true;
  traceEvents?: boolean;
  noHostShares?: boolean;
  root?: boolean;
  forwards?: Array<PortForward | string>;
  vhostUserFs?: Array<ReadonlyVhostUserFsMount | string>;
};

export type SpawnOptions = RunOptions & {
  stdio?: {
    stdin?: "ignore" | "pipe" | "inherit";
    stdout?: "pipe" | "inherit" | "ignore";
    stderr?: "pipe" | "inherit" | "ignore";
  };
};

export type ProcessSpawnOptions = Omit<CommandOptions, "stdin"> & {
  stdin?: "ignore" | "pipe" | "inherit";
  stdout?: "pipe" | "inherit" | "ignore";
  stderr?: "pipe" | "inherit" | "ignore";
};
export type RawSpawnOptions = ProcessSpawnOptions;

export type PortForward = {
  listenPort: number;
  guestPort: number;
  listenHost?: string;
  guestHost?: string;
};

export type ReadonlyVhostUserFsMount = {
  tag: string;
  mount: string;
  socket: string;
};

export type LnxProcess = {
  child: ChildProcess;
  stdin: Writable | null;
  stdout: Readable | null;
  stderr: Readable | null;
  exited: Promise<number>;
  kill(signal?: NodeJS.Signals | number): boolean;
};

export type InstanceSettings = {
  cpus?: number;
  memoryMiB?: number;
  memoryMib?: number;
};

export type InstanceSummary = {
  name: string;
  state: string;
  pids: number[];
};

export type InstanceInspect = {
  name: string;
  state: string;
  pids: number[];
  cpus: number;
  memory_mib: number;
  created: string;
  image: string;
  settings: Record<string, unknown>;
  rootfs: string;
  rootfs_size_bytes: number | null;
  rootfs_allocated_bytes: number | null;
  snapshot: null | {
    path: string;
    pages_allocated_bytes: number | null;
  };
  checkpoints: number;
  descriptor: string;
  logs: {
    run: string;
    console: string;
    owner: string;
  };
};

export type Checkpoint = {
  id?: string;
  name?: string;
  created?: string;
  label: string;
};

export type CheckpointCreateOptions = CommandOptions & {
  message?: string;
};

export type ForkOptions = CommandOptions & {
  checkpoint?: string;
};

export type LogsOptions = CommandOptions & {
  console?: boolean;
  owner?: boolean;
};

export type HostShareEntry = {
  kind: string;
  path: string;
  share: string;
  statePath: string;
  raw: string;
};

export type HostShareState = {
  path?: string;
  share?: string;
  rule?: string;
  match?: string;
  state?: string;
  upper?: string;
  whiteout?: string;
  restore?: string;
  clear?: string;
  raw: string;
};

export type PackageInstallOptions = CommandOptions & {
  builder?: string;
  builderImage?: string;
  packages?: string[];
  binaries?: string[];
};

export type PackagePaths = {
  raw: string;
  [key: string]: string;
};

export type IngressStatus = {
  enabled: boolean;
  raw: string;
  [key: string]: string | boolean;
};

export class LnxCommandError extends Error {
  readonly result: CommandResult;

  constructor(result: CommandResult) {
    super(
      [
        `command failed (${result.status}): ${result.command.join(" ")}`,
        result.stdout ? `stdout:\n${result.stdout}` : undefined,
        result.stderr ? `stderr:\n${result.stderr}` : undefined,
      ]
        .filter(Boolean)
        .join("\n"),
    );
    this.name = "LnxCommandError";
    this.result = result;
  }
}

export function createLnxClient(options: LnxClientOptions = {}): LnxClient {
  return new BinaryLnxClient(options);
}

export type LnxClient = {
  readonly binary: string;
  readonly defaultInstance: string;
  instance(name?: string, options?: Partial<RunOptions>): LnxInstance;
  instances: {
    list(options?: CommandOptions): Promise<InstanceSummary[]>;
  };
  ingress: IngressClient;
  packages: PackageStoreClient;
  cli(args: string[], options?: CommandOptions): Promise<CommandResult>;
};

export type LnxInstance = {
  readonly name: string;
  run(argv: string[], options?: RunOptions): Promise<CommandResult>;
  spawn(argv: string[], options?: SpawnOptions): LnxProcess;
  cli(args: string[], options?: CommandOptions): Promise<CommandResult>;
  spawnCli(args: string[], options?: ProcessSpawnOptions): LnxProcess;
  paths(options?: CommandOptions): Promise<Record<string, string>>;
  inspect(options?: CommandOptions): Promise<InstanceInspect>;
  set(settings: InstanceSettings, options?: CommandOptions): Promise<void>;
  logs(options?: LogsOptions): Promise<string>;
  checkpoint(options?: CheckpointCreateOptions): Promise<Checkpoint>;
  checkpoints(options?: CommandOptions): Promise<Checkpoint[]>;
  fork(targetName: string, options?: ForkOptions): Promise<LnxInstance>;
  snapshots: {
    clear(options?: CommandOptions): Promise<void>;
  };
  fs: {
    unshare(path: string, options?: CommandOptions): Promise<HostShareState>;
    listUnshared(options?: CommandOptions): Promise<HostShareEntry[]>;
    clearUnshared(path: string, options?: CommandOptions): Promise<void>;
  };
};

export type IngressClient = {
  status(options?: CommandOptions): Promise<IngressStatus>;
  enable(options?: CommandOptions): Promise<void>;
  disable(options?: CommandOptions): Promise<void>;
};

export type PackageStoreClient = {
  install(options?: PackageInstallOptions): Promise<void>;
  paths(options?: CommandOptions): Promise<PackagePaths>;
};

class BinaryLnxClient implements LnxClient {
  readonly binary: string;
  readonly defaultInstance: string;
  readonly cwd?: string;
  readonly env?: Environment;

  constructor(options: LnxClientOptions) {
    this.binary = options.binary ?? process.env.LNX_BIN ?? "lnx";
    this.defaultInstance = options.defaultInstance ?? process.env.LNX_INSTANCE ?? "default";
    this.cwd = options.cwd;
    this.env = options.env;
  }

  instance(name = this.defaultInstance, defaults: Partial<RunOptions> = {}): LnxInstance {
    return new BinaryLnxInstance(this, name, defaults);
  }

  get instances() {
    return {
      list: async (options: CommandOptions = {}) => {
        const result = await this.cli(["instances", "list"], options);
        return parseInstances(result.stdout);
      },
    };
  }

  get ingress(): IngressClient {
    return {
      status: async (options: CommandOptions = {}) => parseIngressStatus((await this.cli(["ingress", "status"], options)).stdout),
      enable: async (options: CommandOptions = {}) => {
        await this.cli(["ingress", "enable"], options);
      },
      disable: async (options: CommandOptions = {}) => {
        await this.cli(["ingress", "disable"], options);
      },
    };
  }

  get packages(): PackageStoreClient {
    return {
      install: async (options: PackageInstallOptions = {}) => {
        const args = ["packages", "install"];
        if (options.builder) args.push("--builder", options.builder);
        if (options.builderImage) args.push("--builder-image", options.builderImage);
        for (const binary of options.binaries ?? []) args.push("--bin", binary);
        args.push(...(options.packages ?? []));
        await this.cli(args, options);
      },
      paths: async (options: CommandOptions = {}) => parsePackagePaths((await this.cli(["packages", "paths"], options)).stdout),
    };
  }

  cli(args: string[], options: CommandOptions = {}): Promise<CommandResult> {
    return runCommand(this.binary, args, {
      cwd: options.cwd ?? this.cwd,
      env: { ...this.env, ...options.env },
      stdin: options.stdin,
      timeoutMs: options.timeoutMs,
      check: options.check,
      signal: options.signal,
    });
  }
}

class BinaryLnxInstance implements LnxInstance {
  readonly snapshots;
  readonly fs;

  constructor(
    private readonly client: BinaryLnxClient,
    readonly name: string,
    private readonly defaults: Partial<RunOptions> = {},
  ) {
    this.snapshots = {
      clear: async (options: CommandOptions = {}) => {
        await this.cli(["snapshots", "clear"], options);
      },
    };
    this.fs = {
      unshare: async (path: string, options: CommandOptions = {}) => parseHostShareState((await this.cli(["fs", "unshare", path], options)).stdout),
      listUnshared: async (options: CommandOptions = {}) => parseHostShareEntries((await this.cli(["fs", "unshare", "--list"], options)).stdout),
      clearUnshared: async (path: string, options: CommandOptions = {}) => {
        await this.cli(["fs", "unshare", "--remove", path], options);
      },
    };
  }

  run(argv: string[], options: RunOptions = {}): Promise<CommandResult> {
    const merged = { ...this.defaults, ...options };
    return this.cli([...runOptionArgs(merged), ...argv], {
      ...merged,
      cwd: merged.processCwd ?? merged.cwd,
    });
  }

  spawn(argv: string[], options: SpawnOptions = {}): LnxProcess {
    const merged = { ...this.defaults, ...options };
    return this.spawnCli([...runOptionArgs(merged), ...argv], {
      cwd: merged.processCwd ?? merged.cwd,
      env: merged.env,
      timeoutMs: merged.timeoutMs,
      check: merged.check,
      signal: merged.signal,
      stdin: merged.stdio?.stdin,
      stdout: merged.stdio?.stdout,
      stderr: merged.stdio?.stderr,
    });
  }

  cli(args: string[], options: CommandOptions = {}): Promise<CommandResult> {
    return runCommand(
      this.client.binary,
      ["--instance", this.name, ...args],
      {
        ...options,
        timeoutMs: options.timeoutMs ?? this.defaults.timeoutMs,
        cwd: options.cwd ?? this.client.cwd,
        env: { ...this.client.env, ...this.defaults.env, ...options.env },
      },
      this.name,
    );
  }

  spawnCli(args: string[], options: ProcessSpawnOptions = {}): LnxProcess {
    return spawnCommand(this.client.binary, ["--instance", this.name, ...args], {
      ...options,
      cwd: options.cwd ?? this.client.cwd,
      env: { ...this.client.env, ...options.env },
    });
  }

  async paths(options: CommandOptions = {}): Promise<Record<string, string>> {
    return parseKeyValueOutput((await this.cli(["paths"], options)).stdout);
  }

  async inspect(options: CommandOptions = {}): Promise<InstanceInspect> {
    return JSON.parse((await this.cli(["inspect"], options)).stdout) as InstanceInspect;
  }

  async set(settings: InstanceSettings, options: CommandOptions = {}): Promise<void> {
    const args = ["set"];
    if (settings.cpus !== undefined) args.push(`cpus=${settings.cpus}`);
    const memory = settings.memoryMiB ?? settings.memoryMib;
    if (memory !== undefined) args.push(`memory-mib=${memory}`);
    await this.cli(args, options);
  }

  async logs(options: LogsOptions = {}): Promise<string> {
    const args = ["logs"];
    if (options.console) args.push("--console");
    if (options.owner) args.push("--owner");
    return (await this.cli(args, options)).stdout;
  }

  async checkpoint(options: CheckpointCreateOptions = {}): Promise<Checkpoint> {
    const args = ["checkpoint"];
    if (options.message) args.push("-m", options.message);
    const label = (await this.cli(args, options)).stdout;
    return { label, name: options.message };
  }

  async checkpoints(options: CommandOptions = {}): Promise<Checkpoint[]> {
    return parseCheckpoints((await this.cli(["checkpoints"], options)).stdout);
  }

  async fork(targetName: string, options: ForkOptions = {}): Promise<LnxInstance> {
    const args = ["fork"];
    if (options.checkpoint) args.push("--checkpoint", options.checkpoint);
    args.push(targetName);
    await this.cli(args, options);
    return this.client.instance(targetName, this.defaults);
  }
}

export async function runCommand(
  command: string,
  args: string[],
  options: CommandOptions = {},
  instance?: string,
): Promise<CommandResult> {
  const proc = spawnCommand(command, args, {
    cwd: options.cwd,
    env: options.env,
    stdin: options.stdin === undefined ? "ignore" : "pipe",
    stdout: "pipe",
    stderr: "pipe",
    timeoutMs: options.timeoutMs,
    signal: options.signal,
  });
  if (options.stdin !== undefined) {
    proc.stdin?.end(options.stdin);
  }
  const [status, stdout, stderr] = await Promise.all([
    proc.exited,
    readStream(proc.stdout),
    readStream(proc.stderr),
  ]);
  const result = {
    status,
    exitCode: status,
    stdout: stdout.trimEnd(),
    stderr: stderr.trimEnd(),
    command: [command, ...args],
    instance,
  };
  if (options.check !== false && result.status !== 0) {
    throw new LnxCommandError(result);
  }
  return result;
}

export function spawnCommand(command: string, args: string[], options: ProcessSpawnOptions = {}): LnxProcess {
  const child = spawnChild(command, args, {
    cwd: options.cwd,
    env: mergeEnv(options.env),
    stdio: [options.stdin ?? "ignore", options.stdout ?? "pipe", options.stderr ?? "pipe"],
  });
  const exited = processExit(child, [command, ...args], options);
  return {
    child,
    stdin: child.stdin ?? null,
    stdout: child.stdout ?? null,
    stderr: child.stderr ?? null,
    exited,
    kill: (signal?: NodeJS.Signals | number) => child.kill(signal),
  };
}

function runOptionArgs(options: RunOptions): string[] {
  const args: string[] = [];
  if (options.cwd) args.push("-C", options.cwd);
  if (options.kernel) args.push("--kernel", options.kernel);
  if (options.rootfs) args.push("--rootfs", options.rootfs);
  if (options.cpus !== undefined) args.push("--cpus", String(options.cpus));
  const memory = options.memoryMiB ?? options.memoryMib;
  if (memory !== undefined) args.push("--memory-mib", String(memory));
  if (options.snapshot) args.push("--snapshot", options.snapshot);
  if (options.nestedKvm) args.push("--nested-kvm");
  if (options.deterministic) {
    args.push("--deterministic");
    if (options.deterministic !== true) args.push(options.deterministic);
  }
  if (options.traceEvents) args.push("--trace-events");
  if (options.noHostShares) args.push("--no-host-shares");
  if (options.root) args.push("--root");
  for (const forward of options.forwards ?? []) args.push("--forward", formatForward(forward));
  for (const mount of options.vhostUserFs ?? []) args.push("--vhost-user-fs", formatVhostUserFs(mount));
  return args;
}

function formatForward(forward: PortForward | string): string {
  if (typeof forward === "string") return forward;
  if (forward.listenHost || forward.guestHost) {
    return `${forward.listenHost ?? "127.0.0.1"}:${forward.listenPort}:${forward.guestHost ?? "127.0.0.1"}:${forward.guestPort}`;
  }
  return `${forward.listenPort}:${forward.guestPort}`;
}

function formatVhostUserFs(mount: ReadonlyVhostUserFsMount | string): string {
  if (typeof mount === "string") return mount;
  return `tag=${mount.tag},mount=${mount.mount},socket=${mount.socket},ro`;
}

function mergeEnv(env: Environment | undefined): NodeJS.ProcessEnv {
  const merged: NodeJS.ProcessEnv = { ...process.env };
  for (const [key, value] of Object.entries(env ?? {})) {
    if (value === undefined) {
      delete merged[key];
    } else {
      merged[key] = String(value);
    }
  }
  return merged;
}

async function processExit(
  child: ChildProcess,
  command: string[],
  options: Pick<CommandOptions, "timeoutMs" | "signal">,
): Promise<number> {
  let timeout: NodeJS.Timeout | undefined;
  const onAbort = () => child.kill("SIGTERM");
  if (options.signal) {
    if (options.signal.aborted) onAbort();
    options.signal.addEventListener("abort", onAbort, { once: true });
  }
  try {
    const exit = once(child, "exit") as Promise<[number | null, NodeJS.Signals | null]>;
    const error = once(child, "error").then(([err]) => {
      throw err;
    });
    const deadline = options.timeoutMs
      ? new Promise<never>((_, reject) => {
          timeout = setTimeout(() => {
            child.kill("SIGKILL");
            reject(new Error(`timeout after ${options.timeoutMs}ms: ${command.join(" ")}`));
          }, options.timeoutMs);
        })
      : undefined;
    const [code, signal] = await Promise.race([exit, error, ...(deadline ? [deadline] : [])]);
    return code ?? signalStatus(signal);
  } finally {
    if (timeout) clearTimeout(timeout);
    options.signal?.removeEventListener("abort", onAbort);
  }
}

function signalStatus(signal: NodeJS.Signals | null): number {
  if (!signal) return 1;
  const signalNumbers: Partial<Record<NodeJS.Signals, number>> = {
    SIGHUP: 1,
    SIGINT: 2,
    SIGQUIT: 3,
    SIGILL: 4,
    SIGTRAP: 5,
    SIGABRT: 6,
    SIGBUS: 7,
    SIGFPE: 8,
    SIGKILL: 9,
    SIGUSR1: 10,
    SIGSEGV: 11,
    SIGUSR2: 12,
    SIGPIPE: 13,
    SIGALRM: 14,
    SIGTERM: 15,
  };
  const number = signalNumbers[signal];
  return number ? 128 + number : 1;
}

async function readStream(stream: Readable | null): Promise<string> {
  if (!stream) return "";
  const chunks: Buffer[] = [];
  for await (const chunk of stream) {
    chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  }
  return Buffer.concat(chunks).toString("utf8");
}

function parseKeyValueOutput(stdout: string): Record<string, string> {
  const result: Record<string, string> = {};
  for (const line of stdout.split("\n")) {
    const [key, ...rest] = line.split(":");
    if (!key || rest.length === 0) continue;
    result[key.trim().replaceAll("-", "_")] = rest.join(":").trim();
  }
  return result;
}

function parseInstances(stdout: string): InstanceSummary[] {
  return stdout
    .split("\n")
    .slice(1)
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line) => {
      const [name = "", state = "", pids = ""] = line.split(/\s+/, 3);
      return {
        name,
        state,
        pids: pids
          .split(",")
          .filter(Boolean)
          .map((pid) => Number(pid))
          .filter(Number.isFinite),
      };
    });
}

function parseCheckpoints(stdout: string): Checkpoint[] {
  return stdout
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line) => {
      const [id, maybeName, maybeCreated] = line.split("\t");
      if (maybeCreated) return { id, name: maybeName, created: maybeCreated, label: maybeName };
      return { id, created: maybeName, label: id ?? "" };
    });
}

function parseHostShareEntries(stdout: string): HostShareEntry[] {
  if (stdout.startsWith("no host-share copy-on-write state")) return [];
  return stdout
    .split("\n")
    .map((raw) => {
      const [kind = "", path = "", sharePart = "", statePart = ""] = raw.split("\t");
      return {
        kind,
        path,
        share: sharePart.replace(/^share=/, ""),
        statePath: statePart.replace(/^state=/, ""),
        raw,
      };
    })
    .filter((entry) => entry.kind && entry.path);
}

function parseHostShareState(stdout: string): HostShareState {
  const state: HostShareState = { raw: stdout };
  for (const line of stdout.split("\n")) {
    const [key, ...rest] = line.split(":");
    if (!key || rest.length === 0) continue;
    const value = rest.join(":").trim();
    switch (key.trim()) {
      case "path":
        state.path = value;
        break;
      case "share":
        state.share = value;
        break;
      case "rule":
        state.rule = value;
        break;
      case "match":
        state.match = value;
        break;
      case "state":
        state.state = value;
        break;
      case "upper":
        state.upper = value;
        break;
      case "whiteout":
        state.whiteout = value;
        break;
      case "restore":
        state.restore = value;
        break;
      case "clear":
        state.clear = value;
        break;
    }
  }
  return state;
}

function parseIngressStatus(stdout: string): IngressStatus {
  const parsed = parseKeyValueOutput(stdout) as IngressStatus;
  parsed.raw = stdout;
  parsed.enabled = stdout.split("\n")[0]?.trim() === "enabled";
  return parsed;
}

function parsePackagePaths(stdout: string): PackagePaths {
  return { raw: stdout, ...parseKeyValueOutput(stdout) };
}

export function defaultLnxBinary(): string {
  return process.env.LNX_BIN ?? join(process.env.HOME ?? ".", ".cargo/bin/lnx");
}
