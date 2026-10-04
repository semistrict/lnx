// Records a demo video: an agent drives Chromium inside an lnx VM through
// cua-driver (https://github.com/trycua/cua), lnx forks the running VM, and
// the agent keeps working in both copies with the same browser session.
//
//   bun run build
//   bun scripts/demo/cua-fork/demo.ts [output.mp4]           record, then compose
//   bun scripts/demo/cua-fork/demo.ts compose [output.mp4]   compose again from saved parts
//
// Instances live under LNX_BASE (default ~/.lnx-demo), never ~/.lnx. The
// first run provisions `cua-base` (Xvfb, Openbox, Chromium, cua-driver,
// ffmpeg) and checkpoints it; later runs start from that checkpoint. The raw
// recordings and caption times are kept next to the output in <name>-parts/.

import { mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join, resolve } from "node:path";

const here = import.meta.dir;
const repoRoot = resolve(here, "../../..");
const lnxBin = resolve(Bun.env.LNX_BIN ?? join(repoRoot, "target/debug/lnx"));
const lnxBase = Bun.env.LNX_BASE ?? join(homedir(), ".lnx-demo");
const composeOnly = process.argv[2] === "compose";
const output = resolve(process.argv[composeOnly ? 3 : 2] ?? "cua-fork-demo.mp4");
const partsDir = output.replace(/\.mp4$/, "") + "-parts";

const BASE = "cua-base";
const SOURCE = "cua-demo";
const FORK = "cua-demo-fork";
const PROVISIONED = "provisioned";
const SESSION = "demo";
const PAGE = "http://127.0.0.1:8000/";
// Pause between agent actions so a viewer can follow them.
const BEAT_MS = 1800;

type Vm = typeof SOURCE | typeof FORK;
type Lane = "source" | "fork" | "both";
interface Caption {
  at: number;
  lane: Lane;
  text: string;
}
interface Marks {
  forkCommand: number;
  forked: number;
  end: number;
  forkSeconds: number;
}

const captions: Caption[] = [];
function caption(lane: Lane, text: string): number {
  const at = Date.now() / 1000;
  captions.push({ at, lane, text });
  console.log(`[${lane}] ${text}`);
  return at;
}

const sleep = (ms: number) => new Promise((done) => setTimeout(done, ms));

async function lnx(
  args: string[],
  options: { stdin?: Blob; stdout?: string } = {},
): Promise<string> {
  const proc = Bun.spawn([lnxBin, ...args], {
    env: { ...Bun.env, LNX_BASE: lnxBase },
    stdin: options.stdin ?? "ignore",
    stdout: options.stdout === undefined ? "pipe" : Bun.file(options.stdout),
    stderr: "pipe",
  });
  const [stdout, stderr, status] = await Promise.all([
    options.stdout === undefined ? new Response(proc.stdout as ReadableStream).text() : "",
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  if (status !== 0) {
    throw new Error(`lnx ${args.join(" ")} exited ${status}\n${stderr}${stdout}`);
  }
  return stdout.trim();
}

// Guest commands run without host shares: files move over stdin and stdout,
// so the VMs do not depend on the host's working directory.
const exec = (vm: string, argv: string[], options: { stdin?: Blob; stdout?: string } = {}) =>
  lnx(["--no-host-shares", "--instance", vm, "--root", "--", ...argv], options);

async function cua(vm: Vm, tool: string, args: Record<string, unknown> = {}): Promise<any> {
  const result = JSON.parse(await exec(vm, ["cua", tool, JSON.stringify({ session: SESSION, ...args })]));
  // Tools report failure either as a non-ok status or as a refused effect.
  if ((result.status !== undefined && result.status !== "ok") || result.effect === "refused") {
    throw new Error(`${tool} on ${vm}: ${JSON.stringify(result)}`);
  }
  return result;
}

async function tarball(dir: string, entries: string[]): Promise<Blob> {
  const tar = Bun.spawn(["tar", "-C", dir, "-cf", "-", ...entries], { stdout: "pipe" });
  const [blob, status] = await Promise.all([new Response(tar.stdout).blob(), tar.exited]);
  if (status !== 0) throw new Error(`tar of ${dir} exited ${status}`);
  return blob;
}

async function instanceNames(): Promise<string[]> {
  const listed = JSON.parse(await lnx(["instances", "list", "--json"]));
  return listed.map((instance: { name: string }) => instance.name);
}

const DEMO_FILES = ["provision.sh", "install-desktop.sh", "desktop.sh", "cua", "cua-desktop.service", "openbox-rc.xml", "www"];

// Copies this directory's guest files into the VM and runs one of its scripts.
async function withDemoFiles(vm: string, script: string): Promise<void> {
  await exec(
    vm,
    ["sh", "-c", `rm -rf /opt/cua-demo/src && mkdir -p /opt/cua-demo/src && tar -xf - -C /opt/cua-demo/src && sh /opt/cua-demo/src/${script} /opt/cua-demo/src >&2`],
    { stdin: await tarball(here, DEMO_FILES) },
  );
}

async function ensureBase(): Promise<void> {
  if ((await instanceNames()).includes(BASE)) {
    const checkpoints = JSON.parse(await lnx(["--instance", BASE, "checkpoints", "--json"]));
    if (checkpoints.some((checkpoint: { name: string | null }) => checkpoint.name === PROVISIONED)) {
      return;
    }
    await lnx(["instances", "delete", BASE]);
  }
  console.log(`provisioning ${BASE} (one time)`);
  await lnx(["create", BASE]);
  await lnx(["--instance", BASE, "set", "cpus=4", "memory-mib=6144"]);
  await withDemoFiles(BASE, "provision.sh");
  await lnx(["--instance", BASE, "stop"]);
  await lnx(["--instance", BASE, "checkpoint", "-m", PROVISIONED]);
}

// A restored guest's wall clock resumes at snapshot time. The page and the
// recordings show wall-clock times, so set it from the host.
async function syncClock(vm: Vm): Promise<void> {
  await exec(vm, ["date", "-s", `@${(Date.now() / 1000).toFixed(3)}`]);
}

// Keeps a VM running for the whole demo; without a client the owner would
// suspend it between commands.
function hold(vm: Vm) {
  return Bun.spawn([lnxBin, "--no-host-shares", "--instance", vm, "--", "sleep", "infinity"], {
    env: { ...Bun.env, LNX_BASE: lnxBase },
    stdout: "ignore",
    stderr: "inherit",
  });
}

async function startDesktop(vm: Vm): Promise<void> {
  const timezone = Intl.DateTimeFormat().resolvedOptions().timeZone;
  await withDemoFiles(vm, "install-desktop.sh");
  await exec(vm, [
    "sh",
    "-c",
    `systemctl set-environment TZ=${timezone} && systemctl restart cua-desktop && ` +
      `for i in $(seq 100); do curl -fsS ${PAGE} >/dev/null 2>&1 && cua status >/dev/null 2>&1 && exit 0; sleep 0.1; done; ` +
      `systemctl status --no-pager cua-desktop; exit 1`,
  ]);
}

const RECORDING = "/var/tmp/cua-record.mkv";

// x11grab at 30 fps, stamped with wall-clock time so the original's and the
// fork's recordings line up with each other and with the caption times. The
// recorder keeps running through the fork, so the fork's copy continues too.
async function startRecording(vm: Vm): Promise<void> {
  await exec(vm, [
    "systemd-run", "--unit=cua-record", "--uid=ubuntu", "--gid=ubuntu", "--setenv=DISPLAY=:1",
    "--property=KillSignal=SIGINT", "--",
    "ffmpeg", "-nostdin", "-loglevel", "error",
    "-f", "x11grab", "-framerate", "30", "-video_size", "1280x800", "-draw_mouse", "0", "-i", ":1",
    "-vf", "setpts=RTCTIME/(TB*1000000)", "-fps_mode", "passthrough",
    "-c:v", "libx264", "-preset", "ultrafast", "-crf", "16", "-pix_fmt", "yuv420p", "-y", RECORDING,
  ]);
}

async function stopRecording(vm: Vm, path: string): Promise<void> {
  await exec(vm, ["systemctl", "stop", "cua-record"]);
  await exec(vm, ["cat", RECORDING], { stdout: path });
}

interface Tab {
  target_id: string;
  tab_id: string;
}

async function launchBrowser(vm: Vm): Promise<Tab> {
  const prepared = await cua(vm, "browser_prepare", { allow_launch: true, profile: { mode: "isolated_new" } });
  const pid = prepared.prepared_pid;
  for (let attempt = 0; attempt < 100; attempt++) {
    const { windows } = await cua(vm, "list_windows", { pid });
    if (windows.length > 0) {
      const bound = await cua(vm, "get_browser_state", { pid, window_id: windows[0].window_id });
      return { target_id: bound.target_id, tab_id: bound.tabs[0].tab_id };
    }
    await sleep(100);
  }
  throw new Error(`Chromium (pid ${pid}) opened no window on ${vm}`);
}

type Ref = { ref: string; role: string; name: string | null };

interface Page {
  // The page id is random per page load and the note count lives only in
  // the page's memory, so equal values in two VMs mean one running page.
  pageId: string;
  notes: string;
  // Refs for the note field and button. Every snapshot supersedes the refs
  // of the previous one, so they are only good until the next inspect().
  note: string;
  add: string;
}

async function inspect(vm: Vm, tab: Tab): Promise<Page> {
  // `refs` holds the actionable elements, `content_refs` the page's text.
  const state: { refs: Ref[]; content_refs: Ref[] } = await cua(vm, "get_browser_state", {
    ...tab,
    snapshot_format: "semantic_v2",
  });
  const control = (role: string, name: string) => {
    const match = state.refs.find((item) => item.role === role && item.name === name);
    if (!match) throw new Error(`no ${role} "${name}" on ${vm}: ${JSON.stringify(state.refs)}`);
    return match.ref;
  };
  const texts = state.content_refs.flatMap((item) => (item.name ? [item.name] : []));
  const valueAfter = (label: string) => {
    const index = texts.indexOf(label);
    if (index < 0 || index + 1 >= texts.length) throw new Error(`no "${label}" value on ${vm}: ${JSON.stringify(texts)}`);
    return texts[index + 1];
  };
  return {
    pageId: valueAfter("PAGE ID"),
    notes: valueAfter("NOTES"),
    note: control("textbox", "Note"),
    add: control("button", "Add note"),
  };
}

async function addNote(vm: Vm, tab: Tab, page: Page, text: string): Promise<void> {
  await cua(vm, "browser_type", { ...tab, ref: page.note, text, mode: "keystrokes" });
  await sleep(BEAT_MS / 2);
  // Trusted input may activate the browser window, which nobody else uses
  // inside the VM.
  await cua(vm, "browser_click", { ...tab, ref: page.add, delivery_mode: "foreground" });
}

async function record(): Promise<void> {
  await ensureBase();
  for (const vm of [FORK, SOURCE]) {
    if ((await instanceNames()).includes(vm)) await lnx(["instances", "delete", vm]);
  }
  await lnx(["create", SOURCE, "--from", `${BASE}:${PROVISIONED}`]);
  await rm(partsDir, { recursive: true, force: true });
  await mkdir(partsDir, { recursive: true });

  const holders = [hold(SOURCE)];
  try {
    await syncClock(SOURCE);
    await startDesktop(SOURCE);
    await startRecording(SOURCE);
    await sleep(1000);

    caption("source", "An agent drives Chromium inside an lnx VM with cua-driver");
    await sleep(BEAT_MS);
    caption("source", "browser_prepare: launch Chromium with a fresh profile");
    const tab = await launchBrowser(SOURCE);
    await sleep(BEAT_MS);
    caption("source", `browser_navigate ${PAGE}`);
    await cua(SOURCE, "browser_navigate", { ...tab, url: PAGE });
    await sleep(BEAT_MS);
    const page = await inspect(SOURCE, tab);
    caption("source", "browser_type + browser_click: the agent takes notes");
    await addNote(SOURCE, tab, page, "Research how lnx forks a running VM");
    await sleep(BEAT_MS);
    await addNote(SOURCE, tab, page, "Two ideas to try: plan A and plan B");
    await sleep(BEAT_MS);
    const before = await inspect(SOURCE, tab);
    if (before.notes !== "2") throw new Error(`expected 2 notes before the fork, saw ${before.notes}`);

    const forkCommand = caption("both", `$ lnx --instance ${SOURCE} fork ${FORK}`);
    const forkStarted = performance.now();
    await lnx(["--instance", SOURCE, "fork", FORK]);
    const forkSeconds = (performance.now() - forkStarted) / 1000;
    const forked = Date.now() / 1000;
    holders.push(hold(FORK));
    await Promise.all([syncClock(SOURCE), syncClock(FORK)]);

    const [original, after] = await Promise.all([inspect(SOURCE, tab), inspect(FORK, tab)]);
    if (after.pageId !== before.pageId || after.notes !== before.notes) {
      throw new Error(`fork lost page state: before ${JSON.stringify(before)}, after ${JSON.stringify(after)}`);
    }
    caption("both", `Both VMs show page ${after.pageId} with its ${after.notes} notes and running timer: Chromium never restarted`);
    await sleep(BEAT_MS * 2);
    caption("source", "Keeps going with plan A");
    caption("fork", "Same cua-driver session, tries plan B");
    await sleep(BEAT_MS / 2);
    await Promise.all([
      addNote(SOURCE, tab, original, "Original: going with plan A"),
      addNote(FORK, tab, after, "Fork: trying plan B instead"),
    ]);
    await sleep(BEAT_MS * 1.5);
    caption("source", "browser_navigate: Virtual machine");
    caption("fork", "browser_navigate: fork (system call)");
    await Promise.all([
      cua(SOURCE, "browser_navigate", { ...tab, url: "https://en.wikipedia.org/wiki/Virtual_machine" }),
      cua(FORK, "browser_navigate", { ...tab, url: "https://en.wikipedia.org/wiki/Fork_(system_call)" }),
    ]);
    await sleep(BEAT_MS * 2.5);
    const end = Date.now() / 1000;

    await Promise.all([
      stopRecording(SOURCE, join(partsDir, "source.mkv")),
      stopRecording(FORK, join(partsDir, "fork.mkv")),
    ]);
    const marks: Marks = { forkCommand, forked, end, forkSeconds };
    await writeFile(join(partsDir, "timeline.json"), JSON.stringify({ captions, marks }, null, 2));
  } finally {
    for (const holder of holders) holder.kill();
    await Promise.all(holders.map((holder) => holder.exited));
  }
}

// ---- composition -----------------------------------------------------------

const W = 1920;
const H = 1080;
const FONT = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf";
const BOLD = "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf";
const MONO = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf";
const BACKGROUND = "0x0b1020";
const ENCODE = "-c:v libx264 -preset medium -crf 20 -pix_fmt yuv420p -r 30";

interface Text {
  file: string;
  font: string;
  size: number;
  color: string;
  x: string;
  y: string;
  from?: number;
  to?: number;
}

// drawtext reads text from files, so captions need no filter escaping.
class Texts {
  files = new Map<string, string>();
  file(text: string): string {
    const name = `text-${this.files.size}.txt`;
    this.files.set(name, text);
    return name;
  }
}

function drawtext(text: Text): string {
  const enable = text.from === undefined ? "" : `:enable='between(t,${text.from.toFixed(3)},${(text.to ?? 1e9).toFixed(3)})'`;
  return `drawtext=fontfile=${text.font}:textfile=${text.file}:fontsize=${text.size}:fontcolor=${text.color}:x=${text.x}:y=${text.y}${enable}`;
}

// The captions written during [start, stop), in segment time. A lane
// caption shows until the next one in its lane; a "both" caption gives way
// to the next caption in any lane.
function laneCaptions(all: Caption[], lane: Lane, start: number, stop: number): Array<{ text: string; from: number; to: number }> {
  const inSegment = all.filter((c) => c.at >= start && c.at < stop);
  const successors = lane === "both" ? inSegment : inSegment.filter((c) => c.lane === lane);
  return inSegment
    .filter((c) => c.lane === lane)
    .map((c) => ({
      text: c.text,
      from: c.at - start,
      to: (successors.find((next) => next.at > c.at)?.at ?? stop) - start,
    }));
}

function title(texts: Texts): string {
  return drawtext({
    file: texts.file("lnx  ×  cua-driver: fork a VM with a running browser agent"),
    font: BOLD, size: 40, color: "white", x: "64", y: "36",
  });
}

function composeScript(timeline: { captions: Caption[]; marks: Marks }, starts: { source: number; fork: number }, texts: Texts): string {
  const { captions: all, marks } = timeline;
  const introStart = all[0].at - 1;
  const introEnd = marks.forkCommand + 1.2;
  const splitStart = marks.forked;
  const splitEnd = marks.end;
  const commands: string[] = [];

  // 1. The agent at work in one VM.
  {
    const lines = laneCaptions(all, "source", introStart, introEnd).concat(laneCaptions(all, "both", introStart, introEnd));
    const filters = [
      `[0:v]setpts=PTS-STARTPTS,fps=30,scale=1440:900:flags=lanczos[v]`,
      `color=c=${BACKGROUND}:s=${W}x${H}:r=30[bg]`,
      `[bg][v]overlay=x=240:y=112:shortest=1,` +
        [
          title(texts),
          ...lines.map((line) => drawtext({ file: texts.file(line.text), font: line.text.startsWith("$") ? MONO : FONT, size: 34, color: "white", x: "(w-tw)/2", y: "1028", from: line.from, to: line.to })),
        ].join(",") +
        `,fade=t=in:st=0:d=0.5`,
    ];
    commands.push(
      `ffmpeg -y -v error -ss ${(introStart - starts.source).toFixed(3)} -t ${(introEnd - introStart).toFixed(3)} -i source.mkv ` +
        `-filter_complex "${filters.join(";")}" ${ENCODE} seg1.mp4`,
    );
  }

  // 2. The fork, over a dimmed still of the moment it happened.
  {
    const still = marks.forkCommand + 1.0 - starts.source;
    commands.push(`ffmpeg -y -v error -ss ${still.toFixed(3)} -i source.mkv -frames:v 1 still.png`);
    const filters = [
      `[0:v]scale=1440:900:flags=lanczos,colorchannelmixer=rr=0.25:gg=0.25:bb=0.25[v]`,
      `color=c=${BACKGROUND}:s=${W}x${H}:r=30[bg]`,
      `[bg][v]overlay=x=240:y=112:shortest=1,` +
        [
          title(texts),
          `drawbox=x=200:y=370:w=1520:h=330:color=${BACKGROUND}@0.92:t=fill`,
          drawtext({ file: texts.file(`$ lnx --instance ${SOURCE} fork ${FORK}`), font: MONO, size: 52, color: "0x7dd3fc", x: "(w-tw)/2", y: "420" }),
          drawtext({ file: texts.file("Checkpoints the running VM's memory and disk, then clones them"), font: FONT, size: 38, color: "white", x: "(w-tw)/2", y: "520" }),
          drawtext({ file: texts.file(`done in ${marks.forkSeconds.toFixed(1)} s`), font: BOLD, size: 46, color: "0x4ade80", x: "(w-tw)/2", y: "600" }),
        ].join(","),
    ];
    commands.push(
      `ffmpeg -y -v error -loop 1 -framerate 30 -t 3.5 -i still.png -filter_complex "${filters.join(";")}" ${ENCODE} seg2.mp4`,
    );
  }

  // 3. Both VMs side by side, each still driven by the agent.
  {
    const left = 32;
    const right = W / 2 + 16;
    const top = 210;
    const lines = [
      ...laneCaptions(all, "source", splitStart, splitEnd).map((line) => ({ ...line, x: `${left}+(928-tw)/2`, y: "820" })),
      ...laneCaptions(all, "fork", splitStart, splitEnd).map((line) => ({ ...line, x: `${right}+(928-tw)/2`, y: "820" })),
      ...laneCaptions(all, "both", splitStart, splitEnd).map((line) => ({ ...line, x: "(w-tw)/2", y: "940" })),
    ];
    const filters = [
      `[0:v]setpts=PTS-STARTPTS,fps=30,scale=928:580:flags=lanczos[a]`,
      `[1:v]setpts=PTS-STARTPTS,fps=30,scale=928:580:flags=lanczos[b]`,
      `color=c=${BACKGROUND}:s=${W}x${H}:r=30[bg]`,
      `[bg][a]overlay=x=${left}:y=${top}:shortest=1[l]`,
      `[l][b]overlay=x=${right}:y=${top}:shortest=1,` +
        [
          title(texts),
          `drawbox=x=${left}:y=${top - 6}:w=928:h=6:color=0x60a5fa:t=fill`,
          `drawbox=x=${right}:y=${top - 6}:w=928:h=6:color=0xf59e0b:t=fill`,
          drawtext({ file: texts.file(`original  ·  ${SOURCE}`), font: BOLD, size: 34, color: "0x60a5fa", x: `${left}`, y: "150" }),
          drawtext({ file: texts.file(`fork  ·  ${FORK}`), font: BOLD, size: 34, color: "0xf59e0b", x: `${right}`, y: "150" }),
          ...lines.map((line) => drawtext({ file: texts.file(line.text), font: FONT, size: 30, color: "white", x: line.x, y: line.y, from: line.from, to: line.to })),
        ].join(",") +
        `,fade=t=out:st=${(splitEnd - splitStart - 0.6).toFixed(3)}:d=0.6`,
    ];
    commands.push(
      `ffmpeg -y -v error -ss ${(splitStart - starts.source).toFixed(3)} -t ${(splitEnd - splitStart).toFixed(3)} -i source.mkv ` +
        `-ss ${(splitStart - starts.fork).toFixed(3)} -t ${(splitEnd - splitStart).toFixed(3)} -i fork.mkv ` +
        `-filter_complex "${filters.join(";")}" ${ENCODE} seg3.mp4`,
    );
  }

  commands.push(`printf "file seg1.mp4\\nfile seg2.mp4\\nfile seg3.mp4\\n" > segments.txt`);
  commands.push(`ffmpeg -y -v error -f concat -safe 0 -i segments.txt -c copy -movflags +faststart out.mp4`);
  return ["set -eu", ...commands].join("\n") + "\n";
}

async function compose(): Promise<void> {
  const timeline = JSON.parse(await readFile(join(partsDir, "timeline.json"), "utf8"));
  const starts = { source: 0, fork: 0 };
  const parts = await tarball(partsDir, ["source.mkv", "fork.mkv"]);
  const probe = await exec(
    SOURCE,
    [
      "sh",
      "-c",
      "rm -rf /tmp/compose && mkdir /tmp/compose && cd /tmp/compose && tar -xf - && " +
        "for f in source.mkv fork.mkv; do ffprobe -v error -show_entries format=start_time -of csv=p=0 $f; done",
    ],
    { stdin: parts },
  );
  [starts.source, starts.fork] = probe.split("\n").map(Number);
  const texts = new Texts();
  const script = composeScript(timeline, starts, texts);
  const work = join(partsDir, "compose");
  await rm(work, { recursive: true, force: true });
  await mkdir(work, { recursive: true });
  await writeFile(join(work, "compose.sh"), script);
  for (const [name, text] of texts.files) await writeFile(join(work, name), text);
  await exec(SOURCE, ["sh", "-c", "cd /tmp/compose && tar -xf - && sh compose.sh >&2"], {
    stdin: await tarball(work, ["."]),
  });
  await exec(SOURCE, ["cat", "/tmp/compose/out.mp4"], { stdout: output });
  console.log(`wrote ${output}`);
}

if (!composeOnly) await record();
await compose();
