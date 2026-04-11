#!/usr/bin/env -S python3 -u
"""
Fork the VM — like fork(), both sides continue from the same point.

Host-side (recommended):
    lnx fork -- python3 examples/fork.py
    # Parent keeps running. Child gets this script with fork-role=child.

Guest-side (for processes without open sockets):
    curl -X POST --unix-socket /var/run/lnx/control.sock http://localhost/fork
"""

import os
import sys


def fork_role():
    """Returns 'child' if this process was restored by a VM fork, else None."""
    try:
        return open("/var/run/lnx/fork-role").read().strip() or None
    except FileNotFoundError:
        return None


if __name__ == "__main__":
    role = fork_role()
    if role == "child":
        print(f"child  pid={os.getpid()}")
    else:
        print(f"parent pid={os.getpid()}")
