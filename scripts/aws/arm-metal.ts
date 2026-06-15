#!/usr/bin/env bun
import { existsSync } from "node:fs";
import { lstat, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";

type CommandResult = {
  status: number;
  stdout: string;
  stderr: string;
};

type State = {
  instanceId: string;
  region: string;
  instanceType: string;
  keyName: string;
  privateKeyPath: string;
  securityGroupId: string;
  sshUser: string;
  remoteDir: string;
};

type AwsInstance = {
  InstanceId: string;
  InstanceType: string;
  LaunchTime?: string;
  PublicDnsName?: string;
  PublicIpAddress?: string;
  State?: { Name?: string };
};

type Network = {
  vpcId: string;
  subnetIds: string[];
};

const repoRoot = resolve(import.meta.dir, "../..");
const name = env("LNX_AWS_NAME", "lnx-arm-metal");
const region = normalizeRegion(env("LNX_AWS_REGION", "us-east-1"));
const instanceType = env("LNX_AWS_INSTANCE_TYPE", "c6g.metal");
const idleSeconds = numberEnv("LNX_AWS_IDLE_SECONDS", 3600);
const stopWaitSeconds = numberEnv("LNX_AWS_STOP_WAIT_SECONDS", 10 * 60);
const volumeGiB = numberEnv("LNX_AWS_VOLUME_GIB", 200);
const sshUser = env("LNX_AWS_SSH_USER", "ubuntu");
const sshPublicKeyPath = expandHome(env("LNX_AWS_SSH_PUBLIC_KEY", "~/.ssh/id_ed25519.pub"));
const privateKeyPath = expandHome(env("LNX_AWS_SSH_KEY", sshPublicKeyPath.replace(/\.pub$/, "")));
const keyName = env("LNX_AWS_KEY_NAME", `${name}-${process.env.USER ?? "user"}`);
const remoteDir = env("LNX_AWS_REMOTE_DIR", "lnx");
const statePath = expandHome(env("LNX_AWS_STATE", `~/.lnx/aws/${name}-${region}.json`));
const tagKey = "LnxRole";
const tagValue = "arm-metal-test";
const heartbeatMs = numberEnv("LNX_AWS_HEARTBEAT_MS", 5 * 60 * 1000);
const networkCidr = env("LNX_AWS_VPC_CIDR", "10.88.0.0/16");
const ubuntuAmiParameter = "/aws/service/canonical/ubuntu/server/24.04/stable/current/arm64/hvm/ebs-gp3/ami-id";

const command = Bun.argv[2] ?? "help";
const commandArgs = Bun.argv.slice(3);

try {
  switch (command) {
    case "setup":
      await setup();
      break;
    case "run":
      await syncAndRun(commandArgs.join(" "));
      break;
    case "counter-proof":
      await counterProof(commandArgs);
      break;
    case "snapshot-put":
      await putSnapshot(commandArgs);
      break;
    case "status":
      await status();
      break;
    case "start":
      await start();
      break;
    case "stop":
      await stop();
      break;
    case "terminate":
      await terminate();
      break;
    case "help":
    case "--help":
    case "-h":
      usage();
      break;
    default:
      throw new Error(`unknown command: ${command}`);
  }
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
}

async function setup(): Promise<void> {
  await requireTool("aws");
  await requireTool("ssh");
  await requireTool("rsync");
  await assertAwsCredentials();

  const existing = await currentInstance();
  if (existing) {
    const state = await stateFromInstance(existing);
    await saveState(state);
    const running = await ensureInstanceRunning(state);
    printInstance("existing", running, state);
    return;
  }

  if (!existsSync(sshPublicKeyPath)) {
    throw new Error(`missing SSH public key: ${sshPublicKeyPath}`);
  }
  if (!existsSync(privateKeyPath)) {
    throw new Error(`missing SSH private key: ${privateKeyPath}`);
  }

  await ensureKeyPair();
  const network = await ensureNetwork();
  const securityGroupId = await ensureSecurityGroup(network.vpcId);
  const amiId = await ubuntuAmiId();
  const rootDeviceName = await amiRootDeviceName(amiId);
  const subnets = network.subnetIds;
  const userData = await writeTempUserData();

  try {
    let lastError: unknown;
    for (const subnetId of subnets) {
      try {
        const launched = await runInstance(amiId, rootDeviceName, subnetId, securityGroupId, userData);
        const instanceId = launched.Instances?.[0]?.InstanceId;
        if (!instanceId) {
          throw new Error(`run-instances returned no instance id: ${JSON.stringify(launched)}`);
        }
        await aws(["ec2", "wait", "instance-running", "--instance-ids", instanceId]);
        const instance = await describeInstance(instanceId);
        const state = await stateFromInstance(instance);
        await saveState(state);
        printInstance("created", instance, state);
        return;
      } catch (error) {
        lastError = error;
        console.error(`launch failed in subnet ${subnetId}; trying the next subnet`);
        console.error(String(error instanceof Error ? error.message : error));
      }
    }
    throw lastError ?? new Error("launch failed in every candidate subnet");
  } finally {
    await rm(dirname(userData), { recursive: true, force: true });
  }
}

async function syncAndRun(remoteCommand: string): Promise<void> {
  if (!remoteCommand.trim()) {
    throw new Error("usage: bun run aws:arm:run -- '<command>'");
  }
  const state = await requireRunningState();

  await waitForSsh(state);
  await withLeaseHeartbeat(state, async () => {
    await remote(state, "cloud-init status --wait >/dev/null 2>&1 || true", { quiet: false });
    await installIdleStopScripts(state);
    await remote(state, `mkdir -p ${shellQuote(state.remoteDir)} && sudo /usr/local/bin/lnx-metal-touch ${idleSeconds}`);
    await syncRepo(state);
    await runRemoteCommand(state, remoteCommand);
  });
}

async function putSnapshot(args: string[]): Promise<void> {
  const localArg = args[0];
  if (!localArg) {
    throw new Error("usage: bun run aws:arm:snapshot-put -- <local-snapshot-dir> [remote-snapshot-dir]");
  }
  await requireTool("python3");

  const localSnapshot = resolve(localArg);
  if (!existsSync(localSnapshot)) {
    throw new Error(`missing local snapshot directory: ${localSnapshot}`);
  }
  const localStat = await lstat(localSnapshot);
  if (!localStat.isDirectory()) {
    throw new Error(`local snapshot path is not a directory: ${localSnapshot}`);
  }
  const remoteSnapshot = args[1] ?? `~/lnx-snapshots/${basename(localSnapshot)}`;
  const state = await requireRunningState();

  await waitForSsh(state);
  await withLeaseHeartbeat(state, async () => {
    await remote(state, "cloud-init status --wait >/dev/null 2>&1 || true", { quiet: false });
    await installIdleStopScripts(state);
    await remote(state, "command -v python3 >/dev/null");
    await transferSparseDirectory(state, localSnapshot, remoteSnapshot);
  });
  console.log(`snapshot: ${localSnapshot}`);
  console.log(`remote: ${remoteSnapshot}`);
}

async function counterProof(args: string[]): Promise<void> {
  const localSnapshot = resolve(args[0] ?? join(repoRoot, "target", "aws-counter-fixture-v8"));
  const remoteSnapshot = args[1] ?? `~/lnx-snapshots/${basename(localSnapshot)}`;

  await putSnapshot([localSnapshot, remoteSnapshot]);
  await syncAndRun(counterProofRemoteCommand(remoteSnapshot));
}

function counterProofRemoteCommand(remoteSnapshot: string): string {
  const snapshotExpr = remoteSnapshotShellExpr(remoteSnapshot);
  return `export PATH="$HOME/.cargo/bin:$HOME/.bun/bin:$PATH"
linux_headers="$HOME/.lnx-musl-linux-headers"
mkdir -p "$linux_headers"
ln -sfn /usr/include/linux "$linux_headers/linux"
ln -sfn /usr/include/asm-generic "$linux_headers/asm-generic"
ln -sfn /usr/include/aarch64-linux-gnu/asm "$linux_headers/asm"
export CC_LINUX="\${CC_LINUX:-aarch64-linux-musl-gcc -isystem $linux_headers}"
cargo build
snapshot_dir=${snapshotExpr}
proof_dir="$snapshot_dir-proof"
work_dir="$HOME/lnx-snapshots/work-counter-proof"
rm -rf "$work_dir"
mkdir -p "$work_dir"
cp --sparse=always "$snapshot_dir/rootfs.ext4" "$work_dir/rootfs.ext4"
rm -rf "$proof_dir"
stat -c 'remote-snapshot %n size=%s blocks=%b block_size=%B' "$snapshot_dir"/rootfs.ext4 "$snapshot_dir"/pages.img
set +e
LNX_RESTORE_PROOF_SNAPSHOT_DIR="$proof_dir" \\
LNX_RESTORE_PROOF_SNAPSHOT_DELAY_MS=250 \\
LNX_INGRESS_STATE_DIR=/tmp/lnx-disabled-ingress \\
LNX_ROOTFS_BACKEND=block \\
LNX_AGENT_TIMEOUT_MS=5000 \\
LNX_KRUN_LOG_LEVEL=4 \\
./target/debug/lnx \\
  --instance lnx-aws-counter-restore \\
  --no-host-shares \\
  --memory-mib 512 \\
  --cpus 1 \\
  --rootfs "$work_dir/rootfs.ext4" \\
  --snapshot "$snapshot_dir" \\
  sleep 2
restore_status=$?
set -e
printf 'RESTORE_EXIT=%s\\n' "$restore_status"
python3 - "$snapshot_dir/pages.img" "$proof_dir/pages.img" <<'PY'
import mmap
import struct
import sys

marker = b"LNXAWSCOUNTERv8\\0"

def scan(path):
    hits = []
    with open(path, "rb") as f:
        mm = mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ)
        pos = 0
        while True:
            pos = mm.find(marker, pos)
            if pos < 0:
                break
            counter_at = pos + len(marker)
            if counter_at + 8 <= len(mm):
                value = struct.unpack_from("<Q", mm, counter_at)[0]
                if 0 < value < 10**12:
                    hits.append((pos, value))
            pos += 1
        mm.close()
    return hits

source = dict(scan(sys.argv[1]))
proof = dict(scan(sys.argv[2]))
matches = [(offset, before, proof[offset]) for offset, before in source.items() if proof.get(offset, 0) > before]
print("source", sorted(source.items()))
print("proof", sorted(proof.items()))
if not matches:
    raise SystemExit("AWS_COUNTER_MEMORY_RESTORED missing")
offset, before, after = matches[0]
print(f"AWS_COUNTER_MEMORY_RESTORED={before}->{after} offset={offset}")
PY`;
}

function remoteSnapshotShellExpr(remoteSnapshot: string): string {
  if (remoteSnapshot === "~") {
    return '"$HOME"';
  }
  if (remoteSnapshot.startsWith("~/")) {
    return `"$HOME/${remoteSnapshot.slice(2).replace(/(["\\$`])/g, "\\$1")}"`;
  }
  return shellQuote(remoteSnapshot);
}

async function status(): Promise<void> {
  const state = await loadState().catch(() => null);
  const instance = (state ? await describeInstance(state.instanceId).catch(() => null) : null) ?? (await currentInstance());
  if (!instance) {
    console.log(`no ${name} instance found in ${region}`);
    return;
  }
  const resolvedState = state ?? (await stateFromInstance(instance));
  printInstance("status", instance, resolvedState);
  if (instance.State?.Name === "running") {
    await remote(
      resolvedState,
      "if test -s /var/lib/lnx-metal/stop-after; then printf 'stop-after '; date -u -d @$(cat /var/lib/lnx-metal/stop-after); elif test -s /var/lib/lnx-metal/terminate-after; then printf 'legacy-terminate-after '; date -u -d @$(cat /var/lib/lnx-metal/terminate-after); fi",
      { check: false, quiet: false },
    ).catch(() => {});
  }
}

async function stop(): Promise<void> {
  const state = await loadState().catch(() => null);
  const instanceId = state?.instanceId ?? (await currentInstance())?.InstanceId;
  if (!instanceId) {
    console.log(`no ${name} instance found in ${region}`);
    return;
  }
  const instance = await describeInstance(instanceId);
  if (instance.State?.Name === "stopped") {
    console.log(`stopped ${instanceId}`);
    return;
  }
  if (instance.State?.Name === "terminated") {
    await rm(statePath, { force: true });
    console.log(`instance ${instanceId} is terminated`);
    return;
  }
  if (instance.State?.Name !== "stopping") {
    await aws(["ec2", "stop-instances", "--instance-ids", instanceId]);
  }
  const stopped = await waitForInstanceState(instanceId, "stopped", stopWaitSeconds);
  if (stopped) {
    console.log(`stopped ${instanceId}`);
  } else {
    console.log(`stopping ${instanceId}; root EBS volume is preserved, but EC2 did not reach stopped within ${stopWaitSeconds}s`);
  }
}

async function start(): Promise<void> {
  const state = await requireStateOrSetup();
  const instance = await ensureInstanceRunning(state);
  const refreshedState = await stateFromInstance(instance);
  await saveState(refreshedState);
  printInstance("started", instance, refreshedState);
}

async function terminate(): Promise<void> {
  const state = await loadState().catch(() => null);
  const instanceId = state?.instanceId ?? (await currentInstance())?.InstanceId;
  if (!instanceId) {
    await deleteTaggedNetwork();
    console.log(`no ${name} instance found in ${region}`);
    return;
  }
  await aws(["ec2", "terminate-instances", "--instance-ids", instanceId]);
  await aws(["ec2", "wait", "instance-terminated", "--instance-ids", instanceId]);
  await rm(statePath, { force: true });
  await deleteTaggedNetwork();
  console.log(`terminated ${instanceId}`);
}

async function currentInstance(): Promise<AwsInstance | null> {
  const instances = await awsJson<AwsInstance[]>([
    "ec2",
    "describe-instances",
    "--filters",
    `Name=tag:${tagKey},Values=${tagValue}`,
    `Name=tag:Name,Values=${name}`,
    "Name=instance-state-name,Values=pending,running,stopping,stopped",
    "--query",
    "Reservations[].Instances[]",
  ]);
  return instances
    .sort((a, b) => (b.LaunchTime ?? "").localeCompare(a.LaunchTime ?? ""))
    .find((instance) => ["running", "pending", "stopping", "stopped"].includes(instance.State?.Name ?? "")) ?? null;
}

async function stateFromInstance(instance: AwsInstance): Promise<State> {
  const securityGroupId = await securityGroupForInstance(instance.InstanceId);
  return {
    instanceId: instance.InstanceId,
    region,
    instanceType,
    keyName,
    privateKeyPath,
    securityGroupId,
    sshUser,
    remoteDir,
  };
}

async function ensureKeyPair(): Promise<void> {
  const found = await aws([
    "ec2",
    "describe-key-pairs",
    "--key-names",
    keyName,
    "--query",
    "KeyPairs[0].KeyName",
    "--output",
    "text",
  ], { check: false });
  if (found.status === 0 && found.stdout.trim() === keyName) {
    return;
  }
  await aws([
    "ec2",
    "import-key-pair",
    "--key-name",
    keyName,
    "--public-key-material",
    `fileb://${sshPublicKeyPath}`,
  ]);
}

async function ensureNetwork(): Promise<Network> {
  const configured = process.env.LNX_AWS_SUBNET_ID;
  if (configured) {
    const vpcId = (await aws([
      "ec2",
      "describe-subnets",
      "--subnet-ids",
      configured,
      "--query",
      "Subnets[0].VpcId",
      "--output",
      "text",
    ])).stdout.trim();
    return { vpcId, subnetIds: [configured] };
  }

  const defaultVpc = await defaultVpcId();
  if (defaultVpc) {
    return { vpcId: defaultVpc, subnetIds: await candidateSubnets(defaultVpc, true) };
  }

  const existingVpc = (await aws([
    "ec2",
    "describe-vpcs",
    "--filters",
    `Name=tag:${tagKey},Values=${tagValue}`,
    `Name=tag:Name,Values=${name}-vpc`,
    "Name=state,Values=available",
    "--query",
    "Vpcs[0].VpcId",
    "--output",
    "text",
  ])).stdout.trim();
  if (existingVpc && existingVpc !== "None") {
    return { vpcId: existingVpc, subnetIds: await candidateSubnets(existingVpc, false) };
  }

  return createTaggedNetwork();
}

async function defaultVpcId(): Promise<string | null> {
  const result = await aws([
    "ec2",
    "describe-vpcs",
    "--filters",
    "Name=isDefault,Values=true",
    "--query",
    "Vpcs[0].VpcId",
    "--output",
    "text",
  ], { check: false });
  const id = result.stdout.trim();
  return result.status === 0 && id && id !== "None" ? id : null;
}

async function createTaggedNetwork(): Promise<Network> {
  console.error(`creating tagged VPC ${name}-vpc because ${region} has no default VPC`);
  const tags = [
    { Key: "Name", Value: `${name}-vpc` },
    { Key: tagKey, Value: tagValue },
    { Key: "Project", Value: "lnx" },
  ];
  const vpcId = (await aws([
    "ec2",
    "create-vpc",
    "--cidr-block",
    networkCidr,
    "--tag-specifications",
    JSON.stringify([{ ResourceType: "vpc", Tags: tags }]),
    "--query",
    "Vpc.VpcId",
    "--output",
    "text",
  ])).stdout.trim();
  await aws(["ec2", "wait", "vpc-available", "--vpc-ids", vpcId]);
  await aws(["ec2", "modify-vpc-attribute", "--vpc-id", vpcId, "--enable-dns-support", "{\"Value\":true}"]);
  await aws(["ec2", "modify-vpc-attribute", "--vpc-id", vpcId, "--enable-dns-hostnames", "{\"Value\":true}"]);

  const igwId = (await aws([
    "ec2",
    "create-internet-gateway",
    "--tag-specifications",
    JSON.stringify([{ ResourceType: "internet-gateway", Tags: tags }]),
    "--query",
    "InternetGateway.InternetGatewayId",
    "--output",
    "text",
  ])).stdout.trim();
  await aws(["ec2", "attach-internet-gateway", "--vpc-id", vpcId, "--internet-gateway-id", igwId]);

  const routeTableId = (await aws([
    "ec2",
    "create-route-table",
    "--vpc-id",
    vpcId,
    "--tag-specifications",
    JSON.stringify([{ ResourceType: "route-table", Tags: tags }]),
    "--query",
    "RouteTable.RouteTableId",
    "--output",
    "text",
  ])).stdout.trim();
  await aws([
    "ec2",
    "create-route",
    "--route-table-id",
    routeTableId,
    "--destination-cidr-block",
    "0.0.0.0/0",
    "--gateway-id",
    igwId,
  ]);

  const zones = await awsJson<Array<{ Name: string }>>([
    "ec2",
    "describe-availability-zones",
    "--filters",
    "Name=opt-in-status,Values=opt-in-not-required,opted-in",
    "Name=state,Values=available",
    "--query",
    "AvailabilityZones[].{Name:ZoneName}",
  ]);
  const subnetIds: string[] = [];
  for (let i = 0; i < zones.length; i += 1) {
    const zone = zones[i].Name;
    const subnetId = (await aws([
      "ec2",
      "create-subnet",
      "--vpc-id",
      vpcId,
      "--availability-zone",
      zone,
      "--cidr-block",
      subnetCidr(i),
      "--tag-specifications",
      JSON.stringify([
        {
          ResourceType: "subnet",
          Tags: [
            { Key: "Name", Value: `${name}-subnet-${zone}` },
            { Key: tagKey, Value: tagValue },
            { Key: "Project", Value: "lnx" },
          ],
        },
      ]),
      "--query",
      "Subnet.SubnetId",
      "--output",
      "text",
    ])).stdout.trim();
    await aws(["ec2", "modify-subnet-attribute", "--subnet-id", subnetId, "--map-public-ip-on-launch"]);
    await aws(["ec2", "associate-route-table", "--subnet-id", subnetId, "--route-table-id", routeTableId]);
    subnetIds.push(subnetId);
  }
  if (subnetIds.length === 0) {
    throw new Error(`no availability zones found in ${region}`);
  }
  return { vpcId, subnetIds };
}

async function deleteTaggedNetwork(): Promise<void> {
  const vpcs = await awsJson<Array<{ VpcId: string }>>([
    "ec2",
    "describe-vpcs",
    "--filters",
    `Name=tag:${tagKey},Values=${tagValue}`,
    `Name=tag:Name,Values=${name}-vpc`,
    "--query",
    "Vpcs[].{VpcId:VpcId}",
  ]).catch(() => []);
  for (const { VpcId: vpcId } of vpcs) {
    const securityGroups = await awsJson<Array<{ GroupId: string }>>([
      "ec2",
      "describe-security-groups",
      "--filters",
      `Name=vpc-id,Values=${vpcId}`,
      `Name=tag:${tagKey},Values=${tagValue}`,
      "--query",
      "SecurityGroups[].{GroupId:GroupId}",
    ]).catch(() => []);
    for (const { GroupId: groupId } of securityGroups) {
      await aws(["ec2", "delete-security-group", "--group-id", groupId], { check: false });
    }

    const routeTables = await awsJson<Array<{ RouteTableId: string; Associations?: Array<{ RouteTableAssociationId?: string; Main?: boolean }> }>>([
      "ec2",
      "describe-route-tables",
      "--filters",
      `Name=vpc-id,Values=${vpcId}`,
      `Name=tag:${tagKey},Values=${tagValue}`,
      "--query",
      "RouteTables[].{RouteTableId:RouteTableId,Associations:Associations}",
    ]).catch(() => []);
    for (const routeTable of routeTables) {
      for (const association of routeTable.Associations ?? []) {
        if (!association.Main && association.RouteTableAssociationId) {
          await aws(
            ["ec2", "disassociate-route-table", "--association-id", association.RouteTableAssociationId],
            { check: false },
          );
        }
      }
      await aws(["ec2", "delete-route-table", "--route-table-id", routeTable.RouteTableId], { check: false });
    }

    const igws = await awsJson<Array<{ InternetGatewayId: string }>>([
      "ec2",
      "describe-internet-gateways",
      "--filters",
      `Name=attachment.vpc-id,Values=${vpcId}`,
      `Name=tag:${tagKey},Values=${tagValue}`,
      "--query",
      "InternetGateways[].{InternetGatewayId:InternetGatewayId}",
    ]).catch(() => []);
    for (const { InternetGatewayId: igwId } of igws) {
      await aws(["ec2", "detach-internet-gateway", "--internet-gateway-id", igwId, "--vpc-id", vpcId], {
        check: false,
      });
      await aws(["ec2", "delete-internet-gateway", "--internet-gateway-id", igwId], { check: false });
    }

    const subnets = await awsJson<Array<{ SubnetId: string }>>([
      "ec2",
      "describe-subnets",
      "--filters",
      `Name=vpc-id,Values=${vpcId}`,
      `Name=tag:${tagKey},Values=${tagValue}`,
      "--query",
      "Subnets[].{SubnetId:SubnetId}",
    ]).catch(() => []);
    for (const { SubnetId: subnetId } of subnets) {
      await aws(["ec2", "delete-subnet", "--subnet-id", subnetId], { check: false });
    }

    await aws(["ec2", "delete-vpc", "--vpc-id", vpcId], { check: false });
  }
}

async function ensureSecurityGroup(vpcId: string): Promise<string> {
  const groupName = `${name}-ssh`;
  const existing = (await aws([
    "ec2",
    "describe-security-groups",
    "--filters",
    `Name=vpc-id,Values=${vpcId}`,
    `Name=group-name,Values=${groupName}`,
    "--query",
    "SecurityGroups[0].GroupId",
    "--output",
    "text",
  ], { check: false })).stdout.trim();
  const groupId = existing && existing !== "None"
    ? existing
    : (await aws([
    "ec2",
    "create-security-group",
    "--group-name",
        groupName,
        "--description",
        "SSH access for lnx arm metal test host",
    "--vpc-id",
    vpcId,
    "--tag-specifications",
    JSON.stringify([
      {
        ResourceType: "security-group",
        Tags: [
          { Key: "Name", Value: groupName },
          { Key: tagKey, Value: tagValue },
          { Key: "Project", Value: "lnx" },
        ],
      },
    ]),
    "--query",
    "GroupId",
        "--output",
        "text",
      ])).stdout.trim();

  const cidr = env("LNX_AWS_SSH_CIDR", `${await publicIp()}/32`);
  await aws([
    "ec2",
    "authorize-security-group-ingress",
    "--group-id",
    groupId,
    "--ip-permissions",
    JSON.stringify([
      {
        IpProtocol: "tcp",
        FromPort: 22,
        ToPort: 22,
        IpRanges: [{ CidrIp: cidr, Description: "lnx arm metal ssh" }],
      },
    ]),
  ], { check: false });
  return groupId;
}

async function ubuntuAmiId(): Promise<string> {
  const explicit = process.env.LNX_AWS_AMI_ID;
  if (explicit) {
    return explicit;
  }
  return (await aws([
    "ssm",
    "get-parameter",
    "--name",
    ubuntuAmiParameter,
    "--query",
    "Parameter.Value",
    "--output",
    "text",
  ])).stdout.trim();
}

async function amiRootDeviceName(amiId: string): Promise<string> {
  return (await aws([
    "ec2",
    "describe-images",
    "--image-ids",
    amiId,
    "--query",
    "Images[0].RootDeviceName",
    "--output",
    "text",
  ])).stdout.trim();
}

async function candidateSubnets(vpcId: string, defaultOnly: boolean): Promise<string[]> {
  const filters = [`Name=vpc-id,Values=${vpcId}`];
  if (defaultOnly) {
    filters.push("Name=default-for-az,Values=true");
  } else {
    filters.push(`Name=tag:${tagKey},Values=${tagValue}`);
  }
  const subnets = await awsJson<Array<{ SubnetId: string; AvailabilityZone: string; DefaultForAz: boolean }>>([
    "ec2",
    "describe-subnets",
    "--filters",
    ...filters,
    "--query",
    "Subnets[].{SubnetId:SubnetId,AvailabilityZone:AvailabilityZone,DefaultForAz:DefaultForAz}",
  ]);
  if (subnets.length === 0) {
    throw new Error(`no candidate subnets found in ${vpcId}`);
  }
  return subnets.sort((a, b) => a.AvailabilityZone.localeCompare(b.AvailabilityZone)).map((subnet) => subnet.SubnetId);
}

async function runInstance(
  amiId: string,
  rootDeviceName: string,
  subnetId: string,
  securityGroupId: string,
  userData: string,
): Promise<{ Instances?: AwsInstance[] }> {
  return awsJson([
    "ec2",
    "run-instances",
    "--image-id",
    amiId,
    "--instance-type",
    instanceType,
    "--key-name",
    keyName,
    "--subnet-id",
    subnetId,
    "--security-group-ids",
    securityGroupId,
    "--associate-public-ip-address",
    "--instance-initiated-shutdown-behavior",
    "stop",
    "--metadata-options",
    "HttpTokens=required,HttpEndpoint=enabled",
    "--block-device-mappings",
    JSON.stringify([
      {
        DeviceName: rootDeviceName,
        Ebs: {
          VolumeSize: volumeGiB,
          VolumeType: "gp3",
          DeleteOnTermination: true,
        },
      },
    ]),
    "--tag-specifications",
    JSON.stringify([
      {
        ResourceType: "instance",
        Tags: [
          { Key: "Name", Value: name },
          { Key: tagKey, Value: tagValue },
          { Key: "Project", Value: "lnx" },
        ],
      },
      {
        ResourceType: "volume",
        Tags: [
          { Key: "Name", Value: `${name}-root` },
          { Key: tagKey, Value: tagValue },
          { Key: "Project", Value: "lnx" },
        ],
      },
    ]),
    "--user-data",
    `file://${userData}`,
  ]);
}

async function describeInstance(instanceId: string): Promise<AwsInstance> {
  const instances = await awsJson<AwsInstance[]>([
    "ec2",
    "describe-instances",
    "--instance-ids",
    instanceId,
    "--query",
    "Reservations[].Instances[]",
  ]);
  if (instances.length !== 1) {
    throw new Error(`expected one instance for ${instanceId}, got ${instances.length}`);
  }
  return instances[0];
}

async function securityGroupForInstance(instanceId: string): Promise<string> {
  return (await aws([
    "ec2",
    "describe-instances",
    "--instance-ids",
    instanceId,
    "--query",
    "Reservations[0].Instances[0].SecurityGroups[0].GroupId",
    "--output",
    "text",
  ])).stdout.trim();
}

async function syncRepo(state: State): Promise<void> {
  const instance = await describeInstance(state.instanceId);
  const host = instanceHost(instance);
  const sshCommand = ["ssh", ...sshOptions(state)].map(shellQuote).join(" ");
  const list = await writeGitSyncList();
  const nextRemoteDir = `${state.remoteDir}.next`;
  const prevRemoteDir = `${state.remoteDir}.prev`;
  try {
    await remote(
      state,
      [
        `rm -rf ${shellQuote(nextRemoteDir)} ${shellQuote(prevRemoteDir)}`,
        `mkdir -p ${shellQuote(nextRemoteDir)}`,
      ].join("\n"),
    );
    await passthrough([
      "rsync",
      "-az",
      "--delete",
      "--from0",
      "--files-from",
      list,
      "-e",
      sshCommand,
      `${repoRoot}/`,
      `${state.sshUser}@${host}:${nextRemoteDir}/`,
    ]);
    await remote(
      state,
      [
        `rm -rf ${shellQuote(prevRemoteDir)}`,
        `if test -e ${shellQuote(state.remoteDir)}; then mv ${shellQuote(state.remoteDir)} ${shellQuote(prevRemoteDir)}; fi`,
        `mv ${shellQuote(nextRemoteDir)} ${shellQuote(state.remoteDir)}`,
        `rm -rf ${shellQuote(prevRemoteDir)}`,
      ].join("\n"),
    );
  } finally {
    await rm(dirname(list), { recursive: true, force: true });
  }
}

async function transferSparseDirectory(
  state: State,
  localSnapshot: string,
  remoteSnapshot: string,
): Promise<void> {
  const instance = await describeInstance(state.instanceId);
  const host = instanceHost(instance);
  const tempDir = await mkdtemp(join(tmpdir(), "lnx-sparse-transfer-"));
  const senderPath = join(tempDir, "sparse-send.py");
  const receiverPath = join(tempDir, "sparse-receive.py");
  const remoteReceiver = `/tmp/lnx-sparse-receive-${process.pid}-${Date.now()}.py`;

  try {
    await writeFile(senderPath, sparseSenderScript(), { mode: 0o700 });
    await writeFile(receiverPath, sparseReceiverScript(), { mode: 0o700 });
    await remote(
      state,
      `cat > ${shellQuote(remoteReceiver)} <<'PY'\n${await readFile(receiverPath, "utf8")}PY\nchmod 0700 ${shellQuote(remoteReceiver)}`,
    );
    await pipeCommandToCommand(
      ["python3", senderPath, localSnapshot],
      ["ssh", ...sshOptions(state), `${state.sshUser}@${host}`, "python3", remoteReceiver, remoteSnapshot],
    );
  } finally {
    await remote(state, `rm -f ${shellQuote(remoteReceiver)}`, { check: false }).catch(() => {});
    await rm(tempDir, { recursive: true, force: true });
  }
}

async function pipeCommandToCommand(sourceArgs: string[], destArgs: string[]): Promise<void> {
  const sourceCommand = sourceArgs.map(shellQuote).join(" ");
  const destCommand = destArgs.map(shellQuote).join(" ");
  await passthrough(["/bin/bash", "-lc", `set -o pipefail\n${sourceCommand} | ${destCommand}`]);
}

async function writeGitSyncList(): Promise<string> {
  const result = await run(["git", "-C", repoRoot, "ls-files", "--cached", "--others", "--exclude-standard", "-z"]);
  const files: string[] = [];
  for (const file of result.stdout.split("\0")) {
    if (!file) {
      continue;
    }
    const stat = await lstat(join(repoRoot, file)).catch(() => null);
    if (stat?.isFile() || stat?.isSymbolicLink()) {
      files.push(file);
    }
  }
  const dir = await mkdtemp(join(tmpdir(), "lnx-sync-files-"));
  const path = join(dir, "files0");
  await writeFile(path, `${files.join("\0")}\0`);
  return path;
}

async function runRemoteCommand(state: State, remoteCommand: string): Promise<void> {
  const script = `set -euo pipefail
cmd="$(printf '%s' '${Buffer.from(remoteCommand).toString("base64")}' | base64 -d)"
sudo /usr/local/bin/lnx-metal-touch ${idleSeconds}
cleanup() {
  status=$?
  sudo /usr/local/bin/lnx-metal-touch ${idleSeconds} || true
  exit "$status"
}
trap cleanup EXIT
cd "$HOME/${state.remoteDir}"
bash -lc "$cmd"
`;
  await remote(state, script, { quiet: false, stdinMode: "script" });
}

async function withLeaseHeartbeat<T>(state: State, fn: () => Promise<T>): Promise<T> {
  await touchRemote(state);
  const timer = setInterval(() => {
    touchRemote(state).catch((error) => console.error(`lease heartbeat failed: ${error}`));
  }, heartbeatMs);
  try {
    return await fn();
  } finally {
    clearInterval(timer);
    await touchRemote(state).catch(() => {});
  }
}

async function touchRemote(state: State): Promise<void> {
  await remote(state, `sudo /usr/local/bin/lnx-metal-touch ${idleSeconds}`);
}

async function waitForSsh(state: State): Promise<void> {
  const deadline = Date.now() + 10 * 60 * 1000;
  let lastError = "";
  while (Date.now() < deadline) {
    const result = await remote(state, "test -x /usr/local/bin/lnx-metal-touch", { check: false });
    if (result.status === 0) {
      return;
    }
    lastError = result.stderr || result.stdout;
    await sleep(5000);
  }
  throw new Error(`timed out waiting for SSH: ${lastError}`);
}

async function installIdleStopScripts(state: State): Promise<void> {
  await remote(state, `sudo bash -s <<'ROOT'\n${idleStopInstallScript()}\nROOT`, { quiet: false });
}

async function remote(
  state: State,
  script: string,
  options: { check?: boolean; quiet?: boolean; stdinMode?: "command" | "script" } = {},
): Promise<CommandResult> {
  const instance = await describeInstance(state.instanceId);
  const host = instanceHost(instance);
  const stdin = options.stdinMode === "script" ? script : `set -euo pipefail\n${script}\n`;
  const runner = options.quiet === false ? passthrough : run;
  return runner(["ssh", ...sshOptions(state), `${state.sshUser}@${host}`, "bash", "-s"], {
    stdin,
    check: options.check,
  });
}

function sshOptions(state: State): string[] {
  return [
    "-i",
    state.privateKeyPath,
    "-o",
    "StrictHostKeyChecking=accept-new",
    "-o",
    "ServerAliveInterval=30",
    "-o",
    "ConnectTimeout=10",
  ];
}

async function writeTempUserData(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "lnx-arm-metal-"));
  const path = join(dir, "user-data.sh");
  await writeFile(path, userDataScript(), { mode: 0o600 });
  return path;
}

function userDataScript(): string {
  return `#!/bin/bash
set -euxo pipefail
export DEBIAN_FRONTEND=noninteractive

${idleStopInstallScript()}

apt-get update
apt-get install -y \\
  ca-certificates \\
  clang \\
  cmake \\
  curl \\
  docker.io \\
  e2fsprogs \\
  git \\
  libclang-dev \\
  linux-libc-dev \\
  libssl-dev \\
  lld \\
  llvm \\
  musl-tools \\
  pkg-config \\
  protobuf-compiler \\
  python3 \\
  qemu-utils \\
  rsync \\
  unzip \\
  zstd

systemctl enable --now docker || true
usermod -aG docker ${sshUser} || true
usermod -aG kvm ${sshUser} || true

if ! command -v gvproxy >/dev/null 2>&1; then
  curl -fsSL https://github.com/containers/gvisor-tap-vsock/releases/download/v0.8.9/gvproxy-linux-arm64 -o /usr/local/bin/gvproxy
  chmod 0755 /usr/local/bin/gvproxy
fi

if ! command -v bun >/dev/null 2>&1; then
  su - ${sshUser} -c 'curl -fsSL https://bun.sh/install | bash'
fi
if ! su - ${sshUser} -c 'command -v rustup >/dev/null 2>&1'; then
  su - ${sshUser} -c 'curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal'
fi
su - ${sshUser} -c '$HOME/.cargo/bin/rustup target add aarch64-unknown-linux-musl || true'

/usr/local/bin/lnx-metal-touch ${idleSeconds}
`;
}

function idleStopInstallScript(): string {
  return `set -euo pipefail
mkdir -p /var/lib/lnx-metal
cat >/usr/local/bin/lnx-metal-touch <<'EOF'
#!/bin/bash
set -euo pipefail
seconds="\${1:-3600}"
now="$(date +%s)"
mkdir -p /var/lib/lnx-metal
printf '%s\\n' "$now" >/var/lib/lnx-metal/last-touch
printf '%s\\n' "$((now + seconds))" >/var/lib/lnx-metal/stop-after
rm -f /var/lib/lnx-metal/terminate-after
EOF
chmod 0755 /usr/local/bin/lnx-metal-touch

cat >/usr/local/bin/lnx-metal-idle-stop <<'EOF'
#!/bin/bash
set -euo pipefail
deadline="$(cat /var/lib/lnx-metal/stop-after 2>/dev/null || echo 0)"
now="$(date +%s)"
if [ "$now" -ge "$deadline" ]; then
  logger -t lnx-metal "inactivity deadline reached; stopping EC2 instance"
  /sbin/shutdown -h now
fi
EOF
chmod 0755 /usr/local/bin/lnx-metal-idle-stop

cat >/etc/systemd/system/lnx-metal-idle-stop.service <<'EOF'
[Unit]
Description=Stop lnx arm metal test host after inactivity

[Service]
Type=oneshot
ExecStart=/usr/local/bin/lnx-metal-idle-stop
EOF

cat >/etc/systemd/system/lnx-metal-idle-stop.timer <<'EOF'
[Unit]
Description=Check lnx arm metal inactivity deadline

[Timer]
OnBootSec=2min
OnUnitActiveSec=1min
AccuracySec=10s

[Install]
WantedBy=timers.target
EOF

/usr/local/bin/lnx-metal-touch ${idleSeconds}
systemctl daemon-reload
systemctl disable --now lnx-metal-idle-shutdown.timer 2>/dev/null || true
rm -f /etc/systemd/system/lnx-metal-idle-shutdown.service /etc/systemd/system/lnx-metal-idle-shutdown.timer /usr/local/bin/lnx-metal-idle-shutdown
systemctl enable --now lnx-metal-idle-stop.timer
`;
}

function sparseSenderScript(): string {
  return `#!/usr/bin/env python3
import errno
import json
import os
import stat
import sys

CHUNK = 8 * 1024 * 1024
LARGE_SPARSE = 8 * 1024 * 1024 * 1024
root = os.path.abspath(sys.argv[1])
out = sys.stdout.buffer

def send(obj):
    out.write(json.dumps(obj, separators=(",", ":")).encode("utf-8") + b"\\n")

def read_exact(fd, offset, size):
    chunks = []
    remaining = size
    while remaining:
        chunk = os.pread(fd, remaining, offset)
        if not chunk:
            raise RuntimeError(f"short read at offset {offset}")
        chunks.append(chunk)
        offset += len(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)

def send_chunk(rel, offset, data):
    if not any(data):
        return 0
    send({"type": "data", "path": rel, "offset": offset, "size": len(data)})
    out.write(data)
    return len(data)

def scan_file(fd, rel, size):
    sent = 0
    offset = 0
    while offset < size:
        want = min(CHUNK, size - offset)
        data = read_exact(fd, offset, want)
        sent += send_chunk(rel, offset, data)
        offset += want
    return sent

def copy_extents(fd, rel, size, allocated):
    if size == 0:
        return 0
    sparse_source = allocated > 0 and allocated <= size // 2
    if not hasattr(os, "SEEK_DATA") or not hasattr(os, "SEEK_HOLE"):
        if sparse_source or size >= LARGE_SPARSE:
            raise RuntimeError(f"{rel}: SEEK_DATA/SEEK_HOLE unavailable for sparse image")
        return scan_file(fd, rel, size)

    sent = 0
    offset = 0
    while offset < size:
        try:
            data_start = os.lseek(fd, offset, os.SEEK_DATA)
        except OSError as exc:
            if exc.errno == errno.ENXIO:
                break
            if exc.errno in (errno.EINVAL, errno.ENOTSUP):
                if sparse_source or size >= LARGE_SPARSE:
                    raise RuntimeError(f"{rel}: sparse extents unavailable for sparse image") from exc
                return scan_file(fd, rel, size)
            raise
        data_end = min(os.lseek(fd, data_start, os.SEEK_HOLE), size)
        if sparse_source and data_start == 0 and data_end == size:
            raise RuntimeError(f"{rel}: filesystem reported one full-file data extent for sparse source")
        pos = data_start
        while pos < data_end:
            want = min(CHUNK, data_end - pos)
            chunk = read_exact(fd, pos, want)
            sent += send_chunk(rel, pos, chunk)
            pos += want
        offset = data_end
    return sent

if not os.path.isdir(root):
    raise SystemExit(f"not a directory: {root}")

for dirpath, dirnames, filenames in os.walk(root):
    dirnames.sort()
    filenames.sort()
    for name in filenames:
        path = os.path.join(dirpath, name)
        st = os.lstat(path)
        if not stat.S_ISREG(st.st_mode):
            raise SystemExit(f"snapshot contains non-regular file: {path}")
        rel = os.path.relpath(path, root).replace(os.sep, "/")
        allocated = getattr(st, "st_blocks", 0) * 512
        send({
            "type": "file",
            "path": rel,
            "mode": stat.S_IMODE(st.st_mode),
            "size": st.st_size,
            "allocated": allocated,
        })
        fd = os.open(path, os.O_RDONLY)
        try:
            sent = copy_extents(fd, rel, st.st_size, allocated)
        finally:
            os.close(fd)
        send({"type": "end_file", "path": rel})
        print(f"sent {rel} size={st.st_size} allocated={allocated} data={sent}", file=sys.stderr)

send({"type": "done"})
out.flush()
`;
}

function sparseReceiverScript(): string {
  return `#!/usr/bin/env python3
import json
import os
import shutil
import sys

LARGE_SPARSE = 8 * 1024 * 1024 * 1024
CHUNK = 8 * 1024 * 1024
dest = os.path.abspath(os.path.expanduser(sys.argv[1]))
parent = os.path.dirname(dest)
tmp = f"{dest}.next-{os.getpid()}"
backup = f"{dest}.old-{os.getpid()}"
inp = sys.stdin.buffer
files = {}
finished = False

def remove_any(path):
    if not os.path.exists(path) and not os.path.islink(path):
        return
    if os.path.isdir(path) and not os.path.islink(path):
        shutil.rmtree(path)
    else:
        os.unlink(path)

def safe_path(rel):
    if os.path.isabs(rel):
        raise RuntimeError(f"absolute snapshot path rejected: {rel}")
    normalized = os.path.normpath(rel)
    if normalized == ".." or normalized.startswith("../"):
        raise RuntimeError(f"escaping snapshot path rejected: {rel}")
    return normalized.replace("\\\\", "/")

def read_message():
    line = inp.readline()
    if not line:
        raise RuntimeError("unexpected EOF reading sparse transfer metadata")
    return json.loads(line)

def read_exact(size):
    chunks = []
    remaining = size
    while remaining:
        chunk = inp.read(min(CHUNK, remaining))
        if not chunk:
            raise RuntimeError("unexpected EOF reading sparse transfer data")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)

try:
    os.makedirs(parent, exist_ok=True)
    remove_any(tmp)
    os.makedirs(tmp)

    while True:
        msg = read_message()
        msg_type = msg.get("type")
        if msg_type == "done":
            break
        rel = safe_path(msg["path"])
        path = os.path.join(tmp, rel)
        if msg_type == "file":
            os.makedirs(os.path.dirname(path), exist_ok=True)
            handle = open(path, "w+b", buffering=0)
            handle.truncate(int(msg["size"]))
            files[rel] = {
                "handle": handle,
                "size": int(msg["size"]),
                "allocated": int(msg.get("allocated", 0)),
                "mode": int(msg.get("mode", 0o600)),
                "data": 0,
            }
        elif msg_type == "data":
            entry = files[rel]
            size = int(msg["size"])
            offset = int(msg["offset"])
            data = read_exact(size)
            handle = entry["handle"]
            handle.seek(offset)
            handle.write(data)
            entry["data"] += size
        elif msg_type == "end_file":
            entry = files.pop(rel)
            entry["handle"].close()
            os.chmod(path, entry["mode"])
            st = os.stat(path)
            dest_allocated = getattr(st, "st_blocks", 0) * 512
            source_allocated = entry["allocated"]
            source_has_holes = entry["data"] < entry["size"]
            source_reported_sparse = source_allocated < entry["size"]
            must_remain_sparse = entry["size"] >= LARGE_SPARSE and (
                source_has_holes or source_reported_sparse
            )
            if must_remain_sparse:
                allowed = max(
                    entry["data"] * 2,
                    source_allocated * 2,
                    entry["data"] + 512 * 1024 * 1024,
                    source_allocated + 512 * 1024 * 1024,
                )
                if dest_allocated > allowed:
                    raise RuntimeError(
                        f"{rel}: destination is too dense: source_allocated={source_allocated} "
                        f"sent_data={entry['data']} dest_allocated={dest_allocated} size={entry['size']}"
                    )
            print(
                f"received {rel} size={entry['size']} allocated={dest_allocated} data={entry['data']}",
                file=sys.stderr,
            )
        else:
            raise RuntimeError(f"unknown sparse transfer message: {msg_type}")

    for rel, entry in list(files.items()):
        entry["handle"].close()
        raise RuntimeError(f"unfinished file in sparse transfer: {rel}")

    remove_any(backup)
    if os.path.exists(dest) or os.path.islink(dest):
        os.rename(dest, backup)
    os.rename(tmp, dest)
    remove_any(backup)
    finished = True
    print(f"installed sparse snapshot {dest}", file=sys.stderr)
finally:
    if not finished:
        for entry in files.values():
            try:
                entry["handle"].close()
            except Exception:
                pass
        remove_any(tmp)
`;
}

async function requireStateOrSetup(): Promise<State> {
  const state = await loadState().catch(() => null);
  if (state) {
    const instance = await describeInstance(state.instanceId).catch(() => null);
    const instanceState = instance?.State?.Name;
    if (instance && instanceState !== "terminated" && instanceState !== "shutting-down") {
      return state;
    }
    await rm(statePath, { force: true });
  }
  await setup();
  return loadState();
}

async function requireRunningState(): Promise<State> {
  const state = await requireStateOrSetup();
  await ensureInstanceRunning(state);
  return state;
}

async function ensureInstanceRunning(state: State): Promise<AwsInstance> {
  let instance = await describeInstance(state.instanceId);
  const instanceState = instance.State?.Name;
  if (instanceState === "running") {
    return instance;
  }
  if (instanceState === "pending") {
    await aws(["ec2", "wait", "instance-running", "--instance-ids", state.instanceId]);
  } else if (instanceState === "stopping") {
    const stopped = await waitForInstanceState(state.instanceId, "stopped", stopWaitSeconds);
    if (!stopped) {
      throw new Error(`instance ${state.instanceId} is still stopping after ${stopWaitSeconds}s`);
    }
    await aws(["ec2", "start-instances", "--instance-ids", state.instanceId]);
    await aws(["ec2", "wait", "instance-running", "--instance-ids", state.instanceId]);
  } else if (instanceState === "stopped") {
    await aws(["ec2", "start-instances", "--instance-ids", state.instanceId]);
    await aws(["ec2", "wait", "instance-running", "--instance-ids", state.instanceId]);
  } else if (instanceState === "shutting-down" || instanceState === "terminated") {
    await rm(statePath, { force: true });
    await setup();
    const nextState = await loadState();
    return ensureInstanceRunning(nextState);
  } else {
    throw new Error(`instance ${state.instanceId} is ${instanceState ?? "unknown"}, not runnable`);
  }

  instance = await describeInstance(state.instanceId);
  const refreshedState = await stateFromInstance(instance);
  await saveState(refreshedState);
  return instance;
}

async function waitForInstanceState(instanceId: string, target: string, timeoutSeconds: number): Promise<boolean> {
  const deadline = Date.now() + timeoutSeconds * 1000;
  while (Date.now() <= deadline) {
    const instance = await describeInstance(instanceId);
    if (instance.State?.Name === target) {
      return true;
    }
    await sleep(5000);
  }
  return false;
}

async function saveState(state: State): Promise<void> {
  await mkdir(dirname(statePath), { recursive: true });
  await writeFile(statePath, `${JSON.stringify(state, null, 2)}\n`, { mode: 0o600 });
}

async function loadState(): Promise<State> {
  return JSON.parse(await readFile(statePath, "utf8")) as State;
}

function printInstance(label: string, instance: AwsInstance, state: State): void {
  console.log(`${label}: ${instance.InstanceId} ${instance.State?.Name ?? "unknown"} ${instance.InstanceType}`);
  console.log(`region: ${state.region}`);
  const host = instance.PublicDnsName || instance.PublicIpAddress;
  if (host) {
    console.log(`ssh: ssh ${sshOptions(state).map(shellQuote).join(" ")} ${state.sshUser}@${host}`);
    console.log(`repo: ${state.sshUser}@${host}:${state.remoteDir}`);
  } else {
    console.log("ssh: unavailable until the instance is running");
    console.log(`repo: ${state.remoteDir}`);
  }
  console.log(`state: ${statePath}`);
}

function instanceHost(instance: AwsInstance): string {
  const host = instance.PublicDnsName || instance.PublicIpAddress;
  if (!host) {
    throw new Error(`instance ${instance.InstanceId} has no public DNS or IP yet`);
  }
  return host;
}

async function publicIp(): Promise<string> {
  const result = await run(["curl", "-fsSL", "https://checkip.amazonaws.com"]);
  return result.stdout.trim();
}

async function assertAwsCredentials(): Promise<void> {
  try {
    await aws(["sts", "get-caller-identity", "--output", "json"]);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    if (message.includes("sso") || message.includes("SSO")) {
      throw new Error(`AWS SSO credentials are not usable; run aws sso login and retry.\n${message}`);
    }
    throw error;
  }
}

async function requireTool(tool: string): Promise<void> {
  const result = await run(["/bin/sh", "-lc", `command -v ${tool}`], { check: false });
  if (result.status !== 0) {
    throw new Error(`missing required tool: ${tool}`);
  }
}

async function awsJson<T>(args: string[]): Promise<T> {
  const result = await aws([...args, "--output", "json"]);
  return JSON.parse(result.stdout) as T;
}

async function aws(args: string[], options: { check?: boolean } = {}): Promise<CommandResult> {
  return run(["aws", "--region", region, ...args], options);
}

async function passthrough(
  args: string[],
  options: { stdin?: string; check?: boolean } = {},
): Promise<CommandResult> {
  return run(args, { ...options, passthrough: true });
}

async function run(
  args: string[],
  options: { stdin?: string; check?: boolean; passthrough?: boolean } = {},
): Promise<CommandResult> {
  const proc = Bun.spawn(args, {
    cwd: repoRoot,
    stdin: options.stdin === undefined ? "ignore" : "pipe",
    stdout: options.passthrough ? "inherit" : "pipe",
    stderr: options.passthrough ? "inherit" : "pipe",
  });
  if (options.stdin !== undefined && proc.stdin) {
    await proc.stdin.write(options.stdin);
    proc.stdin.end();
  }
  const [status, stdout, stderr] = await Promise.all([
    proc.exited,
    options.passthrough ? Promise.resolve("") : new Response(proc.stdout).text(),
    options.passthrough ? Promise.resolve("") : new Response(proc.stderr).text(),
  ]);
  const result = { status, stdout: stdout.trimEnd(), stderr: stderr.trimEnd() };
  if (options.check !== false && status !== 0) {
    throw new Error(`command failed (${status}): ${args.join(" ")}\nstdout:\n${result.stdout}\nstderr:\n${result.stderr}`);
  }
  return result;
}

function env(key: string, fallback: string): string {
  const value = process.env[key];
  return value && value.length > 0 ? value : fallback;
}

function numberEnv(key: string, fallback: number): number {
  const value = process.env[key];
  if (!value) {
    return fallback;
  }
  const parsed = Number(value);
  if (!Number.isFinite(parsed) || parsed <= 0) {
    throw new Error(`${key} must be a positive number`);
  }
  return parsed;
}

function normalizeRegion(value: string): string {
  return value === "us-east1" ? "us-east-1" : value;
}

function subnetCidr(index: number): string {
  const match = /^(\d+)\.(\d+)\.0\.0\/16$/.exec(networkCidr);
  if (!match) {
    throw new Error(`LNX_AWS_VPC_CIDR must be a /16 like 10.88.0.0/16, got ${networkCidr}`);
  }
  if (index > 255) {
    throw new Error(`too many availability zones for ${networkCidr}`);
  }
  return `${match[1]}.${match[2]}.${index}.0/24`;
}

function expandHome(path: string): string {
  return path === "~" ? homedir() : path.startsWith("~/") ? join(homedir(), path.slice(2)) : path;
}

function shellQuote(value: string): string {
  return `'${value.replace(/'/g, "'\\''")}'`;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function usage(): void {
  console.log(`usage:
  bun run aws:arm:setup
  bun run aws:arm:run -- '<command>'
  bun run aws:arm:counter-proof -- [local-snapshot-dir] [remote-snapshot-dir]
  bun run aws:arm:snapshot-put -- <local-snapshot-dir> [remote-snapshot-dir]
  bun run aws:arm:status
  bun run aws:arm:start
  bun run aws:arm:stop
  bun run aws:arm:terminate

defaults:
  region:        ${region}
  instance type: ${instanceType}
  idle stop:     ${idleSeconds}s
  stop wait:     ${stopWaitSeconds}s
  state:         ${statePath}

environment:
  LNX_AWS_REGION          AWS region, defaults to us-east-1; us-east1 is normalized
  LNX_AWS_INSTANCE_TYPE   instance type, defaults to c6g.metal
  LNX_AWS_IDLE_SECONDS    inactivity lease before EC2 stop, defaults to 3600
  LNX_AWS_STOP_WAIT_SECONDS bounded wait for EC2 stop, defaults to 600
  LNX_AWS_SSH_CIDR        allowed SSH CIDR, defaults to current public IP /32
  LNX_AWS_SSH_PUBLIC_KEY  public key to import, defaults to ~/.ssh/id_ed25519.pub
  LNX_AWS_SSH_KEY         private key for SSH, defaults to matching private key
  LNX_AWS_SUBNET_ID       optional subnet override
  LNX_AWS_AMI_ID          optional Ubuntu arm64 AMI override
`);
}
