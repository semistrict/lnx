# @semistrict/computesdk-lnx

A [ComputeSDK](https://github.com/computesdk/computesdk) provider backed by
[lnx](https://github.com/semistrict/lnx) — local, on-device Linux VMs for
macOS. This lets anything built on ComputeSDK's unified sandbox interface
(`create`, `runCommand`, `getUrl`, filesystem operations, `destroy`, …) run
against lnx instances instead of a cloud sandbox provider.

This package is not published to npm. It lives in the lnx repo, is built and
tested from source, and is meant to be consumed as a local workspace
dependency or vendored directly.

## Requirements

- The `lnx` binary on `PATH`, or point at it explicitly via the `binary`
  config option or the `LNX_BIN` environment variable.
- macOS on Apple Silicon (lnx's only supported host today).
- The first `create()` call for a given machine may download the kernel and
  base rootfs image, which can take a while depending on network speed.

## Usage

Direct provider mode — use the provider instance as your compute entrypoint:

```ts
import { lnx } from "@semistrict/computesdk-lnx";

const compute = lnx({});

const sandbox = await compute.sandbox.create();
const result = await sandbox.runCommand('echo "hello from lnx"');
console.log(result.stdout); // "hello from lnx"

await sandbox.filesystem.writeFile("/tmp/hello.txt", "hi");
console.log(await sandbox.filesystem.readFile("/tmp/hello.txt"));

await sandbox.destroy();
```

Or register it with ComputeSDK's core `compute` singleton alongside other
providers:

```ts
import { compute } from "computesdk";
import { lnx } from "@semistrict/computesdk-lnx";

compute.setConfig({ provider: lnx({}) });

const sandbox = await compute.sandbox.create();
await sandbox.runCommand('echo "hello from lnx"');
await sandbox.destroy();
```

## Config

| Option | Default | Description |
| --- | --- | --- |
| `binary` | `process.env.LNX_BIN ?? "lnx"` | Path to (or name of) the lnx binary. |
| `timeout` | `300_000` | Sandbox timeout reported in `SandboxInfo`, in milliseconds. Not currently enforced against the VM itself. |
| `namePrefix` | `"csdk-"` | Prefix used for auto-generated sandbox names when `create()` is called without an explicit `name`. |

## Limitations

- **No `templateId`/`snapshotId` support yet.** `create()` throws a
  descriptive error if either is passed. lnx's checkpoint/fork system is the
  natural backing for these concepts and will be wired up in a future
  version.
- **Streaming callbacks (`onStdout`/`onStderr`) are untested.** ComputeSDK
  implements streaming generically on top of `runCommand` via its own daemon
  bootstrap (`daemond`), which this provider does not special-case. It may
  work as-is, but has not been exercised against lnx.
- **`envs` passed to `create()` do not survive `getById()`.** lnx has no
  concept of per-instance persisted environment variables, so a sandbox
  handle obtained via `getById` (e.g. after a process restart) has an empty
  `envs` map even if the original `create()` call set some.
- **`getUrl()` requires ingress to be enabled** on the host
  (`sudo lnx ingress enable`). If ingress is disabled, `getUrl()` throws
  rather than returning an unusable URL.
- **`list()` filters out "partial" instances** (directories with no rootfs,
  e.g. left over from an interrupted operation) since they aren't usable
  sandboxes.
