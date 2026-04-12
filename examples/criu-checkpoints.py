#!/usr/bin/env -S python3 -u
"""
Demonstrates lnx CRIU checkpoints and VM fork using a live Python TCP REPL.

This script self-execs inside the VM: on the host it orchestrates the
demo via subprocess; inside the guest it runs a persistent Python REPL
over TCP. The host connects to the REPL via lnx port forwarding.

CRIU checkpoints dump process state to a separate block device on the
Mac (criu.ext4). The host APFS-clones both rootfs.ext4 and criu.ext4
instantly. On restore, the cloned files replace the originals, the VM
boots, and CRIU restores the processes — same PID, same heap.

VM fork clones the running VM into a child instance. Both parent and
child keep running with the same state at the point of fork.

Usage: ./examples/criu-checkpoints.py
"""

import atexit
import io
import json
import os
import shlex
import shutil
import subprocess
import sys
import time
import urllib.request

INST = None
CHILD_INST = None
KEEPER = None       # background lnx process that keeps the daemon alive
CHILD_KEEPER = None # same for fork child
SCRIPT = os.path.abspath(__file__)
REPL_PORT = 9999


# ---------------------------------------------------------------------------
# Guest side: HTTP REPL server running inside the VM
# ---------------------------------------------------------------------------

def start_repl():
    """Start an HTTP REPL server on REPL_PORT."""
    # Close inherited FDs (vsock exec plumbing) so CRIU can dump us.
    import resource
    for fd in range(3, min(resource.getrlimit(resource.RLIMIT_NOFILE)[0], 1024)):
        try:
            os.close(fd)
        except OSError:
            pass

    # Create a new session so we're a session leader for CRIU.
    os.setsid()

    from http.server import HTTPServer, BaseHTTPRequestHandler
    from socketserver import ThreadingMixIn

    class ThreadingHTTPServer(ThreadingMixIn, HTTPServer):
        daemon_threads = True

    ns = {}

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            body = self.rfile.read(int(self.headers["Content-Length"]))
            expr = json.loads(body)["expr"]
            output = eval_line(expr, ns)
            resp = json.dumps({"output": output}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(resp)))
            self.end_headers()
            self.wfile.write(resp)

        def log_message(self, *args):
            pass  # silence request logs

    srv = ThreadingHTTPServer(("0.0.0.0", REPL_PORT), Handler)
    print(f"Python {sys.version.split()[0]} REPL on :{REPL_PORT} (PID {os.getpid()})")
    sys.stdout.flush()
    srv.serve_forever()


def eval_line(line, ns):
    """Evaluate a line and return the REPL output as a string."""
    buf = io.StringIO()
    buf.write(f">>> {line}\n")
    try:
        parts = [p.strip() for p in line.split(";")]
        for part in parts[:-1]:
            exec(compile(part, "<stdin>", "exec"), ns)
        last = parts[-1]
        try:
            result = eval(compile(last, "<stdin>", "eval"), ns)
            if result is not None:
                buf.write(repr(result) + "\n")
        except SyntaxError:
            exec(compile(last, "<stdin>", "exec"), ns)
    except Exception as e:
        buf.write(f"{type(e).__name__}: {e}\n")
    return buf.getvalue()


# ---------------------------------------------------------------------------
# Host side: orchestrates the demo
# ---------------------------------------------------------------------------

def host_main():
    global INST, CHILD_INST
    INST = f"demo-criu-{os.getpid()}"
    atexit.register(cleanup)

    # --- Setup ---
    run("lnx", "clone", INST)

    # Start the REPL inside the VM. We keep a long-running exec session
    # alive (sleep infinity) so the daemon doesn't idle-shutdown while
    # the REPL runs in the background.
    KEEPER = start_keeper(INST)

    wait_for_port(REPL_PORT)
    print()

    # --- Define state in the REPL ---
    repl(REPL_PORT, 'todos = ["buy milk", "write code", "ship feature"]')
    repl(REPL_PORT, 'secret = 42')
    repl(REPL_PORT, 'cache = {"alice": 9001, "bob": 1337}')
    print()
    repl(REPL_PORT, 'todos')
    repl(REPL_PORT, 'secret')
    repl(REPL_PORT, 'cache')
    repl(REPL_PORT, 'import os; os.getpid()')
    print()

    # --- CRIU checkpoint ---
    # Dumps process memory to CRIU block device, host APFS-clones
    # both rootfs.ext4 and criu.ext4 instantly.
    print("--- CRIU checkpoint ---")
    t0 = time.monotonic()
    run("lnx", "--instance", INST, "checkpoints", "create", "--criu",
        "clean-state")
    elapsed = time.monotonic() - t0
    print(f"    checkpoint took {elapsed:.1f}s")
    print()

    # --- Make destructive changes ---
    repl(REPL_PORT, 'todos.append("break prod")')
    repl(REPL_PORT, 'secret = 0')
    repl(REPL_PORT, 'del cache["alice"]')
    print()
    repl(REPL_PORT, 'todos')
    repl(REPL_PORT, 'secret')
    repl(REPL_PORT, 'cache')
    print()

    # --- Stop VM and restore ---
    # Kill the keeper so the daemon can shut down, then replace
    # rootfs + CRIU volume with checkpoint clones and reboot.
    print("--- stop + restore from CRIU checkpoint ---")
    KEEPER.kill()
    KEEPER.wait()
    run("lnx", "--instance", INST, "stop", "--shutdown")
    t0 = time.monotonic()
    run("lnx", "--instance", INST, "checkpoints", "restore", "clean-state")
    elapsed = time.monotonic() - t0
    print(f"    restore took {elapsed:.1f}s")

    # Boot the VM — CRIU auto-restores processes.
    # Reuse start_keeper (REPL will fail to bind since CRIU restored it,
    # but sleep infinity keeps the daemon alive).
    KEEPER = start_keeper(INST)
    wait_for_port(REPL_PORT)
    print()

    # --- Verify: everything is back ---
    repl(REPL_PORT, 'todos')
    repl(REPL_PORT, 'secret')
    repl(REPL_PORT, 'cache')
    repl(REPL_PORT, 'import os; os.getpid()')
    print()

    # --- VM fork ---
    print("--- VM fork (clone running VM into child) ---")
    t0 = time.monotonic()
    result = run("lnx", "--instance", INST, "fork")
    elapsed = time.monotonic() - t0
    print(f"    fork took {elapsed:.1f}s")

    # Parse child instance name from output.
    CHILD_INST = result.strip().split()[-1]
    print()

    # The child has the same REPL with the same state.
    # Keep the child daemon alive and expose its port.
    child_port = REPL_PORT + 1
    CHILD_KEEPER = start_keeper(CHILD_INST)
    # Wait for child VM to boot (keeper triggers daemon spawn).
    # Retry expose until the child daemon is reachable.
    for attempt in range(30):
        result = subprocess.run(
            ["lnx", "expose", f"{CHILD_INST}:{REPL_PORT}", "--as", f":{child_port}"],
            capture_output=True, text=True)
        if result.returncode == 0:
            print(f"+ lnx expose {CHILD_INST}:{REPL_PORT} --as :{child_port}")
            print(result.stdout, end="")
            break
        time.sleep(1)
    wait_for_port(child_port, timeout=15)

    print("--- Parent state (unchanged) ---")
    repl(REPL_PORT, 'todos')
    repl(REPL_PORT, 'secret')
    repl(REPL_PORT, 'import os; os.getpid()')
    print()

    print("--- Child state (forked copy) ---")
    repl(child_port, 'todos')
    repl(child_port, 'secret')
    repl(child_port, 'import os; os.getpid()')
    print()

    # Mutate child — parent is unaffected.
    print("--- Mutate child, verify parent isolation ---")
    repl(child_port, 'todos.append("child only")')
    repl(child_port, 'todos')
    repl(REPL_PORT, 'todos')
    print()

    # Check fork role in child.
    print("--- Fork role detection ---")
    run("lnx", "--instance", CHILD_INST, "lnx-fork-role")
    print()

    print("CRIU checkpoints: process dump to block device, APFS-clone both files.")
    print("VM fork: instant clone of a running VM with full process state.")


def start_keeper(inst):
    """Start a background lnx process that starts the REPL and keeps the
    daemon alive with a long-running exec session."""
    return subprocess.Popen(
        ["lnx", "--instance", inst, "sh", "-c",
         # Python handles setsid + FD closing internally via start-repl.
         f"python3 {shlex.quote(SCRIPT)} start-repl "
         "</dev/null >/dev/null 2>&1 & sleep infinity"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def start_keeper_only(inst):
    """Start a background lnx process that just keeps the daemon alive.
    Used after CRIU restore where the REPL is already restored."""
    return subprocess.Popen(
        ["lnx", "--instance", inst, "sleep", "infinity"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def wait_for_port(port, timeout=30):
    """Wait for a TCP port to become reachable on localhost."""
    import socket
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=1)
            s.close()
            return
        except OSError:
            time.sleep(0.5)
    raise TimeoutError(f"port {port} not reachable after {timeout}s")


def run(*args):
    """Run a command, printing it first (like set -x)."""
    print(f"+ {shlex.join(args)}")
    result = subprocess.run(args, capture_output=True, text=True)
    if result.stdout:
        print(result.stdout, end="")
    if result.stderr:
        print(result.stderr, end="", file=sys.stderr)
    if result.returncode != 0:
        sys.exit(result.returncode)
    return result.stdout


def lnx(*args):
    """Run a command inside the VM."""
    run("lnx", "--instance", INST, *args)


def repl(port, expression, retries=3):
    """Send an expression to the Python REPL via HTTP port forwarding."""
    url = f"http://127.0.0.1:{port}/"
    data = json.dumps({"expr": expression}).encode()
    req = urllib.request.Request(url, data=data,
                                headers={"Content-Type": "application/json"})
    for attempt in range(retries):
        try:
            with urllib.request.urlopen(req, timeout=5) as resp:
                body = json.loads(resp.read())
            print(body["output"], end="")
            return
        except (ConnectionError, OSError):
            if attempt == retries - 1:
                raise
            time.sleep(1)


def cleanup():
    for k in [CHILD_KEEPER, KEEPER]:
        if k:
            k.kill()
            k.wait()
    for inst in [CHILD_INST, INST]:
        if inst:
            subprocess.run(["lnx", "--instance", inst, "stop", "--shutdown"],
                           capture_output=True)
            home = os.path.expanduser("~")
            shutil.rmtree(os.path.join(home, ".lnx", "instances", inst),
                          ignore_errors=True)


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "start-repl":
        start_repl()
    else:
        host_main()
