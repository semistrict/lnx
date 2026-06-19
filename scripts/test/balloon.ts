import { readFile } from "node:fs/promises";
import { join } from "node:path";

import {
  assertContains,
  assertEq,
  cleanupInstance,
  cleanupContext,
  defaultContext,
  lnx,
  prepareContext,
  run,
  testStep,
} from "./lib";

Bun.env.RUST_LOG ??= "krun_devices::virtio::balloon=info";

const ctx = defaultContext("balloon");
const vmArgs = ["--memory-mib", "2048"];

function reportedBytes(log: string): number {
  let total = 0;
  for (const match of log.matchAll(/balloon: reported (\d+) bytes from free-page reporting queue/g)) {
    total += Number(match[1]);
  }
  return total;
}

try {
  await prepareContext(ctx);

  await testStep("virtio balloon negotiates free page reporting without memory pressure", async () => {
    const result = await lnx(ctx, [
      ...vmArgs,
      "bash",
      "-lc",
      String.raw`
set -euo pipefail

balloon=""
for dev in /sys/bus/virtio/devices/*; do
  test -e "$dev/device" || continue
  case "$(cat "$dev/device")" in
    0x0005|0x00000005|00000005|5)
      balloon="$dev"
      break
      ;;
  esac
done

test -n "$balloon"
driver="$(basename "$(readlink "$balloon/driver")")"
features="$(tr -d '\n' < "$balloon/features")"
reporting="$(printf '%s' "$features" | cut -c 6)"
free_page_hint="$(printf '%s' "$features" | cut -c 4)"
mem_total_kib="$(awk '/^MemTotal:/ { print $2 }' /proc/meminfo)"

order="missing"
if test -r /sys/module/page_reporting/parameters/page_reporting_order; then
  order="$(cat /sys/module/page_reporting/parameters/page_reporting_order)"
fi

printf 'device=%s\n' "$(basename "$balloon")"
printf 'driver=%s\n' "$driver"
printf 'feature_reporting=%s\n' "$reporting"
printf 'feature_free_page_hint=%s\n' "$free_page_hint"
printf 'page_reporting_order=%s\n' "$order"
printf 'mem_total_kib=%s\n' "$mem_total_kib"
`,
    ]);

    assertContains(result.stdout, "driver=virtio_balloon", "virtio balloon driver is bound");
    assertContains(result.stdout, "feature_reporting=1", "free page reporting feature is negotiated");
    assertContains(result.stdout, "feature_free_page_hint=0", "unsupported free page hinting is not negotiated");
    const order = result.stdout.match(/^page_reporting_order=(.+)$/m)?.[1];
    if (!order || order === "missing" || order === "-1" || order === "4294967295") {
      throw new Error(`page reporting did not register:\n${result.stdout}`);
    }
    const memTotalKiB = Number(result.stdout.match(/^mem_total_kib=(\d+)$/m)?.[1]);
    if (!Number.isFinite(memTotalKiB) || memTotalKiB < 1_900_000) {
      throw new Error(`balloon target unexpectedly reduced guest memory:\n${result.stdout}`);
    }
  });

  await testStep("freeing guest memory remains healthy with reporting enabled", async () => {
    const result = await lnx(ctx, [
      ...vmArgs,
      "bash",
      "-lc",
      String.raw`
set -euo pipefail

before="$(awk '/^MemAvailable:/ { print $2 }' /proc/meminfo)"
tmp="$(mktemp /dev/shm/lnx-balloon.XXXXXX)"
dd if=/dev/zero of="$tmp" bs=1M count=128 status=none
rm -f "$tmp"
sync
after="$(awk '/^MemAvailable:/ { print $2 }' /proc/meminfo)"

printf 'before=%s\n' "$before"
printf 'after=%s\n' "$after"
printf 'ok\n'
`,
    ]);

    assertContains(result.stdout, "ok", "guest allocation/free completed");
    const before = Number(result.stdout.match(/^before=(\d+)$/m)?.[1]);
    const after = Number(result.stdout.match(/^after=(\d+)$/m)?.[1]);
    assertEq(Number.isFinite(before), true, "before MemAvailable parsed");
    assertEq(Number.isFinite(after), true, "after MemAvailable parsed");
  });

  await testStep("free page reporting advises a meaningful amount of memory", async () => {
    const instance = `${ctx.instance}-reporting`;
    await cleanupInstance(ctx, instance);
    const result = await run(
      [ctx.lnxBin, "--instance", instance, "--memory-mib", "2048", "bash", "-lc", "sleep 8"],
      {
        env: { RUST_LOG: "krun_devices::virtio::balloon=info" },
        timeoutMs: 120_000,
      },
    );
    const ownerLog = await readFile(join(ctx.base, "instances", instance, "owner.log"), "utf8").catch(() => "");
    const reported = reportedBytes(`${result.stdout}\n${result.stderr}\n${ownerLog}`);
    await cleanupInstance(ctx, instance);
    if (reported < 128 * 1024 * 1024) {
      throw new Error(`free-page reporting only advised ${reported} bytes:\n${result.stderr}\n${ownerLog}`);
    }
  });
} finally {
  await cleanupContext(ctx);
}
