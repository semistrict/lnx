#!/usr/bin/env python3
"""
Demonstrates lnx memory checkpoints using a live Python TCP REPL.

This script self-execs inside the VM: on the host it orchestrates the
demo via subprocess; inside the guest it runs a persistent Python REPL
over TCP. The host connects to the REPL via lnx port forwarding.

After checkpoint and restore, the REPL process is still alive — same
PID, same heap, all variables intact.

Usage: ./examples/memory-checkpoints.py
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
SCRIPT = os.path.abspath(__file__)
REPL_PORT = 9999


# ---------------------------------------------------------------------------
# Guest side: HTTP REPL server running inside the VM
# ---------------------------------------------------------------------------

def start_repl():
    """Start an HTTP REPL server on REPL_PORT."""
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
    global INST
    INST = f"demo-pyrepl-{os.getpid()}"
    atexit.register(cleanup)

    # --- Setup ---
    run("lnx", "clone", INST)

    # Start the REPL inside the VM (self-exec with "start-repl" arg).
    lnx("sh", "-c",
        f"nohup python3 {shlex.quote(SCRIPT)} start-repl "
        "> /tmp/repl.log 2>&1 &")

    # Forward the REPL port to the host.
    expose()
    print()

    # --- Define state in the REPL ---
    repl('todos = ["buy milk", "write code", "ship feature"]')
    repl('secret = 42')
    repl('cache = {"alice": 9001, "bob": 1337}')
    print()
    repl('todos')
    repl('secret')
    repl('cache')
    repl('import os; os.getpid()')
    print()

    # --- Memory checkpoint ---
    # The VM hibernates, rootfs+swap are cloned, VM auto-resumes.
    # The command blocks until the VM is back up.
    run("lnx", "--instance", INST, "checkpoints", "create", "--memory",
        "--description", "Python REPL with todos, secret, cache",
        "--tag", "demo", "clean-state")
    expose()
    print()

    # --- Make destructive changes ---
    repl('todos.append("break prod")')
    repl('secret = 0')
    repl('del cache["alice"]')
    print()
    repl('todos')
    repl('secret')
    repl('cache')
    print()

    # --- Restore from checkpoint ---
    # The VM shuts down, files are replaced, VM auto-resumes from checkpoint.
    # The command blocks until the VM is back up.
    run("lnx", "--instance", INST, "checkpoints", "restore", "clean-state")
    expose()
    print()

    # --- Verify: everything is back ---
    # The REPL process is the same — same PID, same heap.
    repl('todos')
    repl('secret')
    repl('cache')
    repl('import os; os.getpid()')
    print()

    # --- Show checkpoint metadata ---
    run("lnx", "--instance", INST, "checkpoints", "list")

    # --- Cleanup ---
    run("lnx", "--instance", INST, "stop", "--shutdown")
    run("lnx", "--instance", INST, "checkpoints", "delete", "clean-state")
    print()
    print("The Python process survived checkpoint and restore with all")
    print("in-memory state intact \u2014 same PID, same variables, same heap.")


def expose():
    """Forward the guest REPL port to the host."""
    run("lnx", "expose", "--wait", f"{INST}:{REPL_PORT}")


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


def repl(expression):
    """Send an expression to the Python REPL via HTTP port forwarding."""
    url = f"http://127.0.0.1:{REPL_PORT}/"
    data = json.dumps({"expr": expression}).encode()
    req = urllib.request.Request(url, data=data,
                                headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=5) as resp:
        body = json.loads(resp.read())
    print(body["output"], end="")


def cleanup():
    subprocess.run(["lnx", "--instance", INST, "stop", "--shutdown"],
                   capture_output=True)
    home = os.path.expanduser("~")
    shutil.rmtree(os.path.join(home, ".lnx", "instances", INST),
                  ignore_errors=True)


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "start-repl":
        start_repl()
    else:
        host_main()
