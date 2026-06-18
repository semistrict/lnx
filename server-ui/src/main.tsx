import React, { useEffect, useMemo, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import {
  Activity,
  Cpu,
  HardDrive,
  Menu,
  Play,
  Power,
  RefreshCw,
  Search,
  Server,
  Square,
  SquareTerminal,
  X,
} from "lucide-react";
import "./styles.css";

type InstanceState = "running" | "starting" | "stopped" | "partial";

type Instance = {
  name: string;
  state: InstanceState;
  pids: number[];
  cpus: number;
  memory_mib: number;
  image: string | null;
  rootfs_size_bytes: number | null;
  rootfs_allocated_bytes: number | null;
  checkpoints: number;
  has_snapshot: boolean;
};

type TerminalSession = {
  id: string;
  instance: string;
};

const stateClasses: Record<InstanceState, string> = {
  running: "bg-emerald-400 text-black",
  starting: "bg-amber-300 text-black",
  stopped: "bg-zinc-700 text-zinc-200",
  partial: "bg-rose-500 text-white",
};

function App() {
  const [instances, setInstances] = useState<Instance[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [terminalSession, setTerminalSession] = useState<TerminalSession | null>(null);
  const [loading, setLoading] = useState(true);
  const [refreshError, setRefreshError] = useState<string | null>(null);
  const [commandError, setCommandError] = useState<string | null>(null);
  const [pendingAction, setPendingAction] = useState<string | null>(null);
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [query, setQuery] = useState("");

  async function refresh() {
    try {
      const response = await fetch("/v1/instances");
      if (!response.ok) {
        throw new Error(await response.text());
      }
      const body = (await response.json()) as { instances: Instance[] };
      setInstances(body.instances);
      setSelected((current) => current ?? body.instances[0]?.name ?? null);
      setRefreshError(null);
    } catch (err) {
      setRefreshError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    void refresh();
    const timer = window.setInterval(() => void refresh(), 3000);
    return () => window.clearInterval(timer);
  }, []);

  const selectedInstance = useMemo(
    () => instances.find((instance) => instance.name === selected) ?? null,
    [instances, selected],
  );

  function openTerminal(instance: string) {
    setTerminalSession({ id: `${instance}-${crypto.randomUUID()}`, instance });
  }

  function closeTerminal() {
    setTerminalSession(null);
  }

  async function runLifecycleAction(instance: Instance, action: "start" | "stop") {
    const key = `${action}:${instance.name}`;
    setPendingAction(key);
    setCommandError(null);
    try {
      const response = await fetch(`/v1/instances/${encodeURIComponent(instance.name)}/${action}`, {
        method: "POST",
      });
      if (!response.ok) {
        const body = await response.json().catch(() => null) as { message?: string } | null;
        throw new Error(body?.message ?? await response.text());
      }
      if (action === "stop") {
        setTerminalSession((current) => (current?.instance === instance.name ? null : current));
      }
      await refresh();
    } catch (err) {
      setCommandError(err instanceof Error ? err.message : String(err));
    } finally {
      setPendingAction(null);
    }
  }

  const error = commandError ?? refreshError;
  const visibleInstances = useMemo(() => {
    const needle = query.trim().toLowerCase();
    if (!needle) {
      return instances;
    }
    return instances.filter((instance) =>
      [instance.name, instance.state, instance.image ?? ""].some((value) =>
        value.toLowerCase().includes(needle),
      ),
    );
  }, [instances, query]);

  function isStartDisabled(instance: Instance) {
    return (
      instance.state === "running" ||
      instance.state === "starting" ||
      instance.state === "partial" ||
      pendingAction != null
    );
  }

  function isStopDisabled(instance: Instance) {
    return (instance.state !== "running" && instance.state !== "starting") || pendingAction != null;
  }

  return (
    <main className="min-h-screen bg-[#090b0c] text-zinc-100">
      <div className="mobile-topbar">
        <button
          className="icon-button"
          aria-label="Open controls"
          title="Open controls"
          onClick={() => setSidebarOpen(true)}
        >
          <Menu size={18} />
        </button>
        <div className="min-w-0">
          <div className="truncate font-mono text-sm text-zinc-100">
            {selectedInstance?.name ?? "lnx server"}
          </div>
          <div className="text-xs text-zinc-500">
            {selectedInstance?.state ?? "no VM selected"}
          </div>
        </div>
      </div>

      {sidebarOpen && (
        <button
          className="sidebar-scrim"
          aria-label="Close controls"
          onClick={() => setSidebarOpen(false)}
        />
      )}

      <div className="app-shell">
        <section className="terminal-workspace">
          {terminalSession ? (
            <TerminalPane key={terminalSession.id} session={terminalSession} onClose={closeTerminal} />
          ) : (
            <div className="terminal-empty terminal-empty-main">
              <SquareTerminal size={42} />
              <span>{selectedInstance?.state === "running" ? "Open a terminal from the sidebar." : "Start a VM to attach a terminal."}</span>
            </div>
          )}
        </section>

        <aside className={`control-sidebar ${sidebarOpen ? "control-sidebar-open" : ""}`}>
          <div className="sidebar-mast">
            <div className="brand-lockup">
              <div className="brand-mark">
                <Server size={20} />
              </div>
              <div className="min-w-0">
                <h1>lnx server</h1>
                <p>VM control plane</p>
              </div>
            </div>
            <div className="sidebar-actions">
              <button className="icon-button" aria-label="Refresh instances" title="Refresh instances" onClick={() => void refresh()}>
                <RefreshCw size={18} />
              </button>
              <button className="icon-button sidebar-close" aria-label="Close controls" title="Close controls" onClick={() => setSidebarOpen(false)}>
                <X size={18} />
              </button>
            </div>
          </div>

          {error && (
            <div className="operator-alert">
              <span>{error}</span>
              {commandError && (
                <button aria-label="Dismiss error" title="Dismiss error" onClick={() => setCommandError(null)}>
                  <X size={14} />
                </button>
              )}
            </div>
          )}

          <label className="search-box">
            <Search size={15} />
            <input
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="Filter VMs"
            />
          </label>

          <div className="instance-stack">
            <div className="section-label">Machines</div>
            {loading ? (
              <div className="empty-state">Loading instances</div>
            ) : instances.length === 0 ? (
              <div className="empty-state">No sandboxes found.</div>
            ) : visibleInstances.length === 0 ? (
              <div className="empty-state">No matches.</div>
            ) : (
              visibleInstances.map((instance) => {
                const isSelected = selected === instance.name;
                const startPending = pendingAction === `start:${instance.name}`;
                const stopPending = pendingAction === `stop:${instance.name}`;
                return (
                  <article
                    key={instance.name}
                    className={`instance-card ${isSelected ? "instance-card-selected" : ""}`}
                  >
                    <button className="instance-row" onClick={() => setSelected(instance.name)}>
                      <span className={`instance-led ${stateClasses[instance.state]}`} />
                      <span className="instance-row-main">
                        <span className="instance-name">{instance.name}</span>
                        <span className="instance-subline">
                          {instance.cpus} CPU / {instance.memory_mib} MiB
                        </span>
                      </span>
                      <span className={`status-pill ${stateClasses[instance.state]}`}>{instance.state}</span>
                    </button>

                    {isSelected && (
                      <div className="instance-expansion">
                        <div className="command-grid">
                          <button
                            className="command-button compact"
                            disabled={isStartDisabled(instance)}
                            onClick={() => void runLifecycleAction(instance, "start")}
                          >
                            <Power size={16} />
                            {startPending ? "Starting" : "Start"}
                          </button>
                          <button
                            className="command-button compact secondary"
                            disabled={isStopDisabled(instance)}
                            onClick={() => void runLifecycleAction(instance, "stop")}
                          >
                            <Square size={15} />
                            {stopPending ? "Stopping" : "Stop"}
                          </button>
                          <button
                            className="command-button compact secondary"
                            disabled={instance.state !== "running"}
                            onClick={() => openTerminal(instance.name)}
                          >
                            <Play size={16} />
                            Terminal
                          </button>
                        </div>

                        <InstanceDetails instance={instance} />
                      </div>
                    )}
                  </article>
                );
              })
            )}
          </div>
        </aside>
      </div>
    </main>
  );
}

function InstanceDetails({ instance }: { instance: Instance }) {
  return (
    <div className="detail-grid">
      <Detail label="PIDs" value={instance.pids.length ? instance.pids.join(", ") : "none"} icon={<Activity size={14} />} />
      <Detail label="Compute" value={`${instance.cpus} CPU / ${instance.memory_mib} MiB`} icon={<Cpu size={14} />} />
      <Detail label="Image" value={instance.image ?? "base rootfs"} />
      <Detail label="Rootfs" value={formatBytes(instance.rootfs_size_bytes)} icon={<HardDrive size={14} />} />
      <Detail label="Allocated" value={formatBytes(instance.rootfs_allocated_bytes)} />
      <Detail label="Checkpoints" value={instance.checkpoints.toString()} />
      <Detail label="Snapshot" value={instance.has_snapshot ? "latest available" : "none"} />
    </div>
  );
}

function Detail({ label, value, icon }: { label: string; value: string; icon?: React.ReactNode }) {
  return (
    <div className="detail-line">
      <div className="detail-label">
        {icon}
        {label}
      </div>
      <div className="detail-value">{value}</div>
    </div>
  );
}

function TerminalPane({
  session,
  onClose,
}: {
  session: TerminalSession;
  onClose: () => void;
}) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const socketRef = useRef<WebSocket | null>(null);

  useEffect(() => {
    const term = new Terminal({
      cursorBlink: true,
      convertEol: true,
      fontFamily: '"IBM Plex Mono", "SFMono-Regular", Consolas, monospace',
      fontSize: 14,
      theme: {
        background: "#050707",
        foreground: "#d7ded9",
        cursor: "#7bf1a8",
        black: "#050707",
        red: "#ff6b6b",
        green: "#7bf1a8",
        yellow: "#ffd166",
        blue: "#5dd6ff",
        magenta: "#d0a2ff",
        cyan: "#72f7d4",
        white: "#f4f7f5",
        brightBlack: "#5b6460",
        brightRed: "#ff8585",
        brightGreen: "#9cffbd",
        brightYellow: "#ffe08a",
        brightBlue: "#8be4ff",
        brightMagenta: "#dfbdff",
        brightCyan: "#a5ffe9",
        brightWhite: "#ffffff",
      },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(containerRef.current!);
    fit.fit();

    const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
    const socket = new WebSocket(`${protocol}//${window.location.host}/v1/instances/${encodeURIComponent(session.instance)}/terminal`);
    socket.binaryType = "arraybuffer";
    socketRef.current = socket;

    const sendResize = () => {
      fit.fit();
      if (socket.readyState === WebSocket.OPEN) {
        socket.send(JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }));
      }
    };

    socket.addEventListener("open", () => {
      term.focus();
      sendResize();
    });
    socket.addEventListener("message", (event) => {
      if (event.data instanceof ArrayBuffer) {
        term.write(new Uint8Array(event.data));
      } else {
        term.write(String(event.data));
      }
    });
    socket.addEventListener("close", () => {
      term.write("\r\n[disconnected]\r\n");
    });
    socket.addEventListener("error", () => {
      term.write("\r\n[terminal websocket error]\r\n");
    });

    const disposable = term.onData((data) => {
      if (socket.readyState === WebSocket.OPEN) {
        socket.send(data);
      }
    });
    const resizeObserver = new ResizeObserver(sendResize);
    resizeObserver.observe(containerRef.current!);

    return () => {
      disposable.dispose();
      resizeObserver.disconnect();
      socket.close();
      term.dispose();
    };
  }, [session.instance]);

  return (
    <div className="overflow-hidden border border-zinc-800 bg-black shadow-terminal">
      <div className="flex items-center justify-between border-b border-zinc-800 bg-[#141819] px-3 py-2">
        <div className="font-mono text-xs text-zinc-300">{session.instance}</div>
        <button className="icon-button small" aria-label="Close terminal" title="Close terminal" onClick={onClose}>
          <X size={15} />
        </button>
      </div>
      <div ref={containerRef} className="terminal-canvas" />
    </div>
  );
}

function formatBytes(value: number | null) {
  if (value == null) {
    return "unknown";
  }
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let next = value;
  let unit = 0;
  while (next >= 1024 && unit < units.length - 1) {
    next /= 1024;
    unit += 1;
  }
  return `${next.toFixed(unit === 0 ? 0 : 1)} ${units[unit]}`;
}

createRoot(document.getElementById("root")!).render(<App />);
