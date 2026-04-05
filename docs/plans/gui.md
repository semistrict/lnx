# GUI App Support (`lnx --gui`)

## Goal

Run Linux GUI apps from lnx with each app appearing as a native macOS window. No full desktop, no single-window compositor — individual app windows integrated into the macOS desktop.

## Architecture

```
+-- macOS Host ------------------------------------------------+
|                                                               |
|  lnx --gui bash -l                                           |
|    +-- terminal session (same as today)                      |
|    +-- cocoa-way (creates NSWindow per Wayland surface)      |
|    +-- waypipe client <----+                                 |
|                             | vsock port 1035                |
+-----------------------------+--------------------------------+
|  Linux Guest                |                                |
|    +-- waypipe server ------+                                |
|    +-- XWayland (X11 app compat)                             |
|    +-- apps connect to WAYLAND_DISPLAY                       |
+--------------------------------------------------------------+
```

### How it works

1. **waypipe** is a Wayland protocol proxy. The guest runs `waypipe server`, which acts as a Wayland compositor stub. It serializes the Wayland protocol and shared memory buffers over a socket. waypipe natively supports vsock for VM transport.

2. **cocoa-way** is a native macOS Wayland compositor written in Rust (Smithay). It receives the Wayland protocol from waypipe client and creates a real NSWindow per Wayland surface, rendered with Metal/OpenGL. Supports HiDPI/Retina, server-side decorations, clipboard.

3. **XWayland** runs inside the guest for X11 app compatibility. X11 apps connect to XWayland, which translates to the Wayland protocol, which waypipe then forwards.

4. The `--gui` flag adds GUI capability alongside the normal terminal. You get your shell, and any GUI app you launch appears as a native macOS window.

### Prior art

- **WSLg** (Microsoft): Modified Weston compositor with RDP backend, per-app windows via RAIL/VAIL. Production-grade but tied to Windows + RDP.
- **Cocoa-Way**: Purpose-built for this exact use case on macOS. Uses waypipe for transport.
- **OrbStack**: No native GUI support yet. Users work around with XQuartz/xrdp.
- **Parallels Coherence**: Per-app windows but proprietary, Windows-only guest.

## Implementation

### 1. Host binary management

Two binaries required on the macOS host, installed via Homebrew:
- **cocoa-way** — macOS Wayland compositor (creates NSWindow per surface)
- **waypipe-darwin** — macOS port of waypipe (deserializes Wayland protocol, connects to cocoa-way)

`lnx --gui` checks PATH for both binaries. If missing, prints install instructions:
```
brew tap J-x-Z/tap && brew install cocoa-way waypipe-darwin
```

### 2. Rootfs changes

Add to rootfs build:
- `waypipe` — Wayland protocol proxy (guest side)
- `xwayland` — X11 compatibility layer
- `foot` — lightweight Wayland terminal (for testing)
- Mesa with software rendering (llvmpipe) — no GPU needed

### 3. New vsock port (1035)

- Add `WaypipePort = 1035` to `internal/protocol/protocol.go` (1034 is SSHAgentPort)
- Carries waypipe wire protocol between guest and host

### 4. Guest init changes

When `Setup.GUI` is true:
- Start `waypipe --vsock server` on vsock port 1034
- Set `WAYLAND_DISPLAY` in the environment for all guest processes
- Start XWayland connected to the waypipe Wayland socket
- Non-fatal: if waypipe isn't installed in rootfs, log a warning and continue without GUI

### 5. Host-side process management

On VM boot when `Config.GUI` is true:
- Accept vsock connection on port 1035
- Start `cocoa-way` (creates Wayland socket in a temp XDG_RUNTIME_DIR)
- Create a unix socket relay between the vsock connection and `waypipe-darwin`
- Start `waypipe-darwin --socket <relay-path> client` with WAYLAND_DISPLAY pointing to cocoa-way
- Both are child processes, killed on VM shutdown

### 6. Config and protocol changes

```go
// config.go
type Config struct {
    // ...existing fields...
    GUI bool // Enable GUI app support (waypipe + cocoa-way)
}

// internal/protocol/protocol.go
const WaypipePort = 1035

// Setup message
type Setup struct {
    // ...existing fields...
    GUI bool // Start waypipe server for GUI app forwarding
}
```

### 7. CLI changes

```go
// cmd/lnx/main.go
var doGUI bool

// In rootCmd flags:
rootCmd.Flags().BoolVar(&doGUI, "gui", false, "enable GUI app support (per-app native macOS windows)")

// In stripLnxFlags:
case a == "--gui":
    doGUI = true
    i++

// In runVM, pass to Config:
GUI: doGUI,
```

## UX

```bash
# Terminal + GUI support
$ lnx --gui
$ firefox &              # appears as native macOS window
$ code .                 # another native window
$ gimp photo.png &       # another window
$ exit                   # VM shuts down, all windows close

# Launch a GUI app directly
$ lnx --gui firefox

# From a second terminal while VM is running
$ lnx exec -- firefox   # launches into existing GUI session
```

## What we explicitly don't need (v1)

- No virtio-gpu device / kernel DRM drivers — waypipe uses CPU rendering (llvmpipe)
- No `StartGraphicApplication` / AppKit run loop in lnx process
- No compositor in guest — waypipe IS the compositor stub
- No VNC, RDP, or X11 forwarding

## Future improvements (v2+)

- **GPU acceleration**: Add virtio-gpu to VM config + kernel DRM, let waypipe use GPU-rendered buffers instead of CPU. Faster rendering for complex apps.
- **Audio**: PulseAudio/PipeWire forwarding over vsock (cocoa-way or separate channel).
- **Clipboard**: Wayland clipboard protocol is forwarded by waypipe/cocoa-way. May need polish.
- **Drag and drop**: Between macOS and Linux app windows.
- **`lnx gui` subcommand**: Attach GUI to an already-running VM instance (connect cocoa-way to existing waypipe session).

## Open questions

- **waypipe vsock handshake**: Need to verify waypipe's `--vsock` mode works directly with Virtualization.framework's vsock, or if we need to proxy through a unix socket on the host side.
- **cocoa-way maturity**: Project is new. Need to evaluate stability, test with common apps (Firefox, VS Code, terminals). May need to contribute fixes upstream.
- **Download URL**: Need to decide hosting. Options: cocoa-way GitHub releases, or build and host ourselves.
- **XWayland startup**: Does waypipe handle XWayland lifecycle, or do we start it separately in guest init?
