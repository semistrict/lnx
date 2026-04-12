# Release Notes

## CRIU Checkpoints and VM Fork

This release adds process-level checkpoint/restore using CRIU (Checkpoint/Restore In Userspace), enabling fast snapshotting and forking of running VMs.

### New features

**CRIU checkpoints** (`lnx checkpoints create --criu <name>`)

Captures the full state of all running processes — memory, file descriptors, TCP connections, pipes, Unix domain sockets — alongside the disk. Restore with `lnx checkpoints restore <name>` to roll back a VM to the exact moment of the checkpoint, including in-flight network connections and in-memory state.

**VM fork** (`lnx fork`)

Clones a running VM into an independent copy. The child VM boots with all processes restored to the same state as the parent at the moment of the fork. The parent continues uninterrupted.

**Guest-initiated fork** (pipe-based, like `fork()`)

Processes inside the VM can trigger a fork by writing to fd 3 and reading the result from fd 4. In the parent, the read returns the child instance name. In the CRIU-restored child, the read returns EOF. See `examples/fork.py` for the pattern.

### Changes

- CRIU images are stored on a dedicated block device (`criu.ext4`), separate from the rootfs. This keeps checkpoint data out of the root filesystem and enables independent cloning.
- CRIU is now built from a local fork (`third_party/criu`) that patches `SO_PASSSEC` handling for kernels without LSM support.
- The rootfs Dockerfile (`Dockerfile.rootfs`) builds CRIU from the local fork instead of cloning upstream.
- Process-level checkpoint/restore is listed alongside disk-only checkpoints in `lnx checkpoints list` (shown as type `criu` vs `disk`).
- The `lnx fork` command can optionally exec into the child VM with `lnx fork -- <command>`.
- Leaked file descriptors (vsock sockets from the VZ framework) are no longer inherited by guest processes, fixing CRIU dump failures on processes that don't explicitly close inherited fds.
- CRIU auto-restore on boot now runs before any process-forking commands (network setup, resize2fs), preventing PID conflicts that would cause restore failures.
