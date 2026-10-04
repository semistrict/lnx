# Fork a VM with a running browser agent

Records a video. Inside an lnx VM, an agent drives Chromium through
[cua-driver](https://github.com/trycua/cua) (MIT). lnx then forks the running
VM, and the agent keeps working in both copies with the same cua-driver
session, the same browser and the same page.

```sh
bun run build
bun run demo:cua-fork -- /path/to/cua-fork-demo.mp4
```

The first run provisions a `cua-base` instance under `~/.lnx-demo` (override
with `LNX_BASE`; the script never uses `~/.lnx`) and checkpoints it. That
takes a few minutes: it installs Xvfb, Openbox, ffmpeg, Chromium from the
xtradeb/apps PPA and a pinned, checksummed cua-driver release. Later runs
start from that checkpoint and take about a minute.

Each run:

1. Creates `cua-demo` from the base and starts the desktop (`cua-desktop.service`)
   and a 30 fps x11grab recording stamped with wall-clock time.
2. Uses cua-driver's browser tools (`browser_prepare`, `browser_navigate`,
   `browser_type`, `browser_click`) to open a scratchpad page and add notes.
   The page shows a random page id and a timer that live only in its memory.
3. Runs `lnx --instance cua-demo fork cua-demo-fork` while the VM is running.
4. Checks that the fork shows the same page id and notes, then adds a
   different note in each VM and sends each to a different site.
5. Pulls both recordings out (the recorder keeps running through the fork,
   so the fork has its own continuation) and composes the video inside the VM.

The raw recordings and caption times are kept in `<output>-parts/`. To
re-render after changing the layout, run
`bun scripts/demo/cua-fork/demo.ts compose <output.mp4>`.

| File | Runs | Purpose |
| --- | --- | --- |
| `demo.ts` | host | orchestration and video composition |
| `provision.sh` | guest, once | packages, Chromium, cua-driver |
| `install-desktop.sh` | guest, every run | installs the files below |
| `desktop.sh`, `cua-desktop.service` | guest | Xvfb, session bus, Openbox, cua-driver daemon, page server |
| `cua` | guest | runs cua-driver as the desktop user |
| `www/index.html` | guest | the scratchpad page |
