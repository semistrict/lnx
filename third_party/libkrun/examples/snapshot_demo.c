/*
 * snapshot_demo: minimal end-to-end test of libkrun's snapshot capture.
 *
 * Boots a libkrun VM against the host's root filesystem (mounted via
 * virtio-fs). The guest writes a readiness marker into that shared rootfs,
 * then sleeps. The helper thread calls krun_snapshot only after observing the
 * marker from the host, so capture is synchronized to a known guest state.
 *
 * Usage:
 *   ./snapshot_demo [--nested] <snapshot_dir>
 *
 * Restore is exercised with:
 *   ./snapshot_demo [--nested] --restore <snapshot_dir>
 * which sets krun_set_snapshot_path before krun_start_enter. The guest should
 * resume mid-`sleep` and the VM should keep running.
 */

#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <libkrun.h>

#define READY_MARKER ".krun_snapshot_ready"
#define DONE_MARKER ".krun_snapshot_done"

struct snap_args {
    uint32_t ctx_id;
    const char *path;
    const char *ready_path;
};

static void *snapshot_thread(void *opaque) {
    struct snap_args *a = (struct snap_args *)opaque;
    fprintf(stderr, "[host] snapshot thread: waiting for guest marker %s\n", a->ready_path);
    for (;;) {
        if (access(a->ready_path, F_OK) == 0) {
            break;
        }
        usleep(1000);
    }
    const char *delay_us = getenv("KRUN_SNAPSHOT_DELAY_AFTER_READY_US");
    if (delay_us && delay_us[0] != '\0') {
        usleep((useconds_t)strtoul(delay_us, NULL, 10));
    }

    fprintf(stderr, "[host] calling krun_snapshot(%u, \"%s\")\n", a->ctx_id, a->path);
    int rc = krun_snapshot(a->ctx_id, a->path);
    if (rc != 0) {
        fprintf(stderr, "[host] krun_snapshot failed: %d (errno-style)\n", rc);
        _exit(1);
    }
    fprintf(stderr, "[host] snapshot captured successfully — exiting host process\n");
    _exit(0);
}

static char *join_path(const char *dir, const char *name) {
    size_t dir_len = strlen(dir);
    size_t name_len = strlen(name);
    int need_slash = dir_len > 0 && dir[dir_len - 1] != '/';
    char *path = malloc(dir_len + need_slash + name_len + 1);
    if (!path) return NULL;
    memcpy(path, dir, dir_len);
    if (need_slash) path[dir_len++] = '/';
    memcpy(path + dir_len, name, name_len + 1);
    return path;
}

int main(int argc, char **argv) {
    int restore_mode = 0;
    int nested_mode = 0;
    const char *snap_dir = NULL;

    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--nested") == 0) {
            nested_mode = 1;
        } else if (strcmp(argv[i], "--restore") == 0) {
            restore_mode = 1;
        } else if (!snap_dir) {
            snap_dir = argv[i];
        } else {
            snap_dir = NULL;
            break;
        }
    }
    if (!snap_dir) {
        fprintf(stderr, "usage: %s [--nested] [--restore] <snapshot_dir>\n", argv[0]);
        return 2;
    }

    const char *log_level = getenv("KRUN_LOG_LEVEL");
    if (krun_set_log_level(log_level ? (uint32_t)strtoul(log_level, NULL, 10) : 3) != 0) {
        fprintf(stderr, "warning: krun_set_log_level failed\n");
    }

    int ctx_id = krun_create_ctx();
    if (ctx_id < 0) {
        fprintf(stderr, "krun_create_ctx failed: %d\n", ctx_id);
        return 1;
    }

    int rc = krun_set_vm_config((uint32_t)ctx_id, 1, 512);
    if (rc != 0) {
        fprintf(stderr, "krun_set_vm_config failed: %d\n", rc);
        return 1;
    }
    if (nested_mode) {
        rc = krun_set_nested_virt((uint32_t)ctx_id, true);
        if (rc != 0) {
            fprintf(stderr, "krun_set_nested_virt failed: %d\n", rc);
            return 1;
        }
    }
    if (getenv("KRUN_DISABLE_VSOCK")) {
        rc = krun_disable_implicit_vsock((uint32_t)ctx_id);
        if (rc != 0) {
            fprintf(stderr, "krun_disable_implicit_vsock failed: %d\n", rc);
            return 1;
        }
    } else if (getenv("KRUN_EXPLICIT_VSOCK_NO_TSI") || !getenv("KRUN_USE_IMPLICIT_VSOCK")) {
        rc = krun_disable_implicit_vsock((uint32_t)ctx_id);
        if (rc != 0) {
            fprintf(stderr, "krun_disable_implicit_vsock failed: %d\n", rc);
            return 1;
        }
        rc = krun_add_vsock((uint32_t)ctx_id, 0);
        if (rc != 0) {
            fprintf(stderr, "krun_add_vsock failed: %d\n", rc);
            return 1;
        }
    }
    const char *listen_port = getenv("KRUN_VSOCK_LISTEN_PORT");
    const char *listen_socket = getenv("KRUN_VSOCK_LISTEN_SOCKET");
    if (listen_port && listen_port[0] != '\0' && listen_socket && listen_socket[0] != '\0') {
        rc = krun_add_vsock_port2((uint32_t)ctx_id,
                                  (uint32_t)strtoul(listen_port, NULL, 10),
                                  listen_socket,
                                  true);
        if (rc != 0) {
            fprintf(stderr, "krun_add_vsock_port2 listen failed: %d\n", rc);
            return 1;
        }
    }
    // Mount a prepared Linux rootfs from the host. Build it with the helper
    // script (see comment at the top of this file). Path is overridable via
    // KRUN_ROOTFS env var.
    const char *root = getenv("KRUN_ROOTFS");
    if (!root) root = "/Users/ramon/krun-rootfs";
    char *ready_path = join_path(root, READY_MARKER);
    char *done_path = join_path(root, DONE_MARKER);
    if (!ready_path || !done_path) {
        fprintf(stderr, "failed to allocate marker paths\n");
        return 1;
    }
    const char *restore_snapshot_path = getenv("KRUN_SNAPSHOT_AFTER_RESTORE_PATH");
    int capture_after_restore = restore_mode &&
        restore_snapshot_path && restore_snapshot_path[0] != '\0';
    if (!restore_mode || capture_after_restore) {
        unlink(ready_path);
    }
    if (!restore_mode) {
        unlink(done_path);
    }

    rc = krun_set_root((uint32_t)ctx_id, root);
    if (rc != 0) {
        fprintf(stderr, "krun_set_root failed: %d\n", rc);
        return 1;
    }

    const char *kernel_path = getenv("KRUN_KERNEL_PATH");
    if (kernel_path && kernel_path[0] != '\0') {
        const char *initrd_path = getenv("KRUN_INITRD_PATH");
        const char *kernel_cmdline = getenv("KRUN_KERNEL_CMDLINE");
        rc = krun_set_kernel((uint32_t)ctx_id, kernel_path, KRUN_KERNEL_FORMAT_RAW,
                             initrd_path && initrd_path[0] != '\0' ? initrd_path : NULL,
                             kernel_cmdline && kernel_cmdline[0] != '\0' ? kernel_cmdline : NULL);
        if (rc != 0) {
            fprintf(stderr, "krun_set_kernel failed: %d\n", rc);
            return 1;
        }
    }

    // libkrun convention: exec_path is the program; the argv array is
    // &argv[1] (not including argv[0]).
    const char *default_script =
        "sleep 60 & p=$!; "
        "printf ready > /" READY_MARKER "; "
        "sync /" READY_MARKER "; "
        "wait $p; "
        "printf done > /" DONE_MARKER "; "
        "sync /" DONE_MARKER;
    const char *script = getenv("KRUN_GUEST_SCRIPT");
    if (!script) script = default_script;
    const char *args[] = { "-c", script, NULL };
    const char *envp_guest[] = { NULL };
    rc = krun_set_exec((uint32_t)ctx_id, "/bin/sh", args, envp_guest);
    if (rc != 0) {
        fprintf(stderr, "krun_set_exec failed: %d\n", rc);
        return 1;
    }

    if (restore_mode) {
        fprintf(stderr, "[host] restore mode: krun_set_snapshot_path(\"%s\")\n", snap_dir);
        rc = krun_set_snapshot_path((uint32_t)ctx_id, snap_dir);
        if (rc != 0) {
            fprintf(stderr, "krun_set_snapshot_path failed: %d\n", rc);
            return 1;
        }
    }

    if (!restore_mode || capture_after_restore) {
        // Spawn a helper thread that will call krun_snapshot after the guest
        // publishes the readiness marker for this capture cycle.
        pthread_t tid;
        static struct snap_args args;
        args.ctx_id = (uint32_t)ctx_id;
        args.path = capture_after_restore ? restore_snapshot_path : snap_dir;
        args.ready_path = ready_path;
        if (pthread_create(&tid, NULL, snapshot_thread, &args) != 0) {
            fprintf(stderr, "pthread_create failed\n");
            return 1;
        }
        pthread_detach(tid);
    }

    fprintf(stderr, "[host] krun_start_enter — handing off to libkrun event loop\n");
    rc = krun_start_enter((uint32_t)ctx_id);
    // krun_start_enter on a successful run calls _exit() itself; if we get
    // here, the VM failed to start.
    fprintf(stderr, "krun_start_enter returned %d (errno-style)\n", rc);
    return rc < 0 ? 1 : 0;
}
