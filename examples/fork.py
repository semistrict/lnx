#!/usr/bin/env -S python3 -u
"""
VM fork with classic fork() semantics.

    lnx python3 examples/fork.py

Like os.fork(), returns in both parent and child:
  - Parent: returns child instance name (truthy)
  - Child:  returns None (CRIU-restored at same program counter)
"""

import os


def fork():
    """Fork the VM. Returns child instance name in parent, None in child.

    Writes "fork" to fd 3 (pipe to init), reads result from fd 4.
    In the CRIU-restored child, fd 4 is dead → returns None.
    """
    try:
        os.write(3, b"fork\n")
        result = os.read(4, 4096)
        if not result:
            return None  # EOF = restored child
        text = result.decode().strip()
        if text.startswith("error:"):
            raise RuntimeError(text)
        return text
    except OSError:
        # Restored child — pipe fds are dead.
        return None


if __name__ == "__main__":
    child = fork()
    pid = os.getpid()

    if child is None:
        print(f"[child]  pid={pid}")
    else:
        print(f"[parent] pid={pid}  child={child}")
