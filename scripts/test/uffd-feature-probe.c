#include <errno.h>
#include <fcntl.h>
#include <linux/userfaultfd.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <unistd.h>

#ifndef UFFD_USER_MODE_ONLY
#define UFFD_USER_MODE_ONLY 1
#endif
#ifndef UFFD_FEATURE_MINOR_HUGETLBFS
#define UFFD_FEATURE_MINOR_HUGETLBFS (1 << 9)
#endif
#ifndef UFFD_FEATURE_MINOR_SHMEM
#define UFFD_FEATURE_MINOR_SHMEM (1 << 10)
#endif
#ifndef UFFD_FEATURE_PAGEFAULT_FLAG_WP
#define UFFD_FEATURE_PAGEFAULT_FLAG_WP (1 << 0)
#endif
#ifndef UFFD_FEATURE_EVENT_REMOVE
#define UFFD_FEATURE_EVENT_REMOVE (1 << 3)
#endif
#ifndef UFFD_FEATURE_MISSING_HUGETLBFS
#define UFFD_FEATURE_MISSING_HUGETLBFS (1 << 4)
#endif
#ifndef UFFD_FEATURE_MISSING_SHMEM
#define UFFD_FEATURE_MISSING_SHMEM (1 << 5)
#endif
#ifndef UFFD_FEATURE_WP_HUGETLBFS_SHMEM
#define UFFD_FEATURE_WP_HUGETLBFS_SHMEM (1 << 12)
#endif
#ifndef UFFD_FEATURE_WP_UNPOPULATED
#define UFFD_FEATURE_WP_UNPOPULATED (1 << 13)
#endif
#ifndef UFFD_FEATURE_WP_ASYNC
#define UFFD_FEATURE_WP_ASYNC (1 << 15)
#endif
#ifndef UFFDIO_REGISTER_MODE_WP
#define UFFDIO_REGISTER_MODE_WP ((__u64)1 << 1)
#endif
#ifndef UFFDIO_WRITEPROTECT_MODE_WP
#define UFFDIO_WRITEPROTECT_MODE_WP ((__u64)1 << 0)
#endif

struct required_feature {
    unsigned long long bit;
    const char *name;
};

static int open_uffd(void) {
    int fd = (int)syscall(SYS_userfaultfd, O_CLOEXEC | UFFD_USER_MODE_ONLY);
    if (fd < 0) {
        fprintf(stderr, "userfaultfd failed: errno=%d (%s)\n", errno, strerror(errno));
    }
    return fd;
}

static int enable_api(int fd, unsigned long long requested, struct uffdio_api *api) {
    memset(api, 0, sizeof(*api));
    api->api = UFFD_API;
    api->features = requested;
    if (ioctl(fd, UFFDIO_API, api) != 0) {
        fprintf(stderr, "UFFDIO_API failed: errno=%d (%s)\n", errno, strerror(errno));
        return 1;
    }
    return 0;
}

static int require_features(unsigned long long features) {
    static const struct required_feature required[] = {
        { UFFD_FEATURE_PAGEFAULT_FLAG_WP, "UFFD_FEATURE_PAGEFAULT_FLAG_WP" },
        { UFFD_FEATURE_EVENT_REMOVE, "UFFD_FEATURE_EVENT_REMOVE" },
        { UFFD_FEATURE_MISSING_HUGETLBFS, "UFFD_FEATURE_MISSING_HUGETLBFS" },
        { UFFD_FEATURE_MISSING_SHMEM, "UFFD_FEATURE_MISSING_SHMEM" },
        { UFFD_FEATURE_MINOR_HUGETLBFS, "UFFD_FEATURE_MINOR_HUGETLBFS" },
        { UFFD_FEATURE_MINOR_SHMEM, "UFFD_FEATURE_MINOR_SHMEM" },
        { UFFD_FEATURE_WP_HUGETLBFS_SHMEM, "UFFD_FEATURE_WP_HUGETLBFS_SHMEM" },
        { UFFD_FEATURE_WP_UNPOPULATED, "UFFD_FEATURE_WP_UNPOPULATED" },
        { UFFD_FEATURE_WP_ASYNC, "UFFD_FEATURE_WP_ASYNC" },
    };
    int missing = 0;

    for (size_t i = 0; i < sizeof(required) / sizeof(required[0]); i++) {
        if ((features & required[i].bit) == 0) {
            fprintf(stderr, "missing %s\n", required[i].name);
            missing = 1;
        }
    }
    return missing;
}

static int probe_sync_wp(void) {
    int fd = open_uffd();
    if (fd < 0) {
        return 1;
    }

    struct uffdio_api api;
    if (enable_api(fd, UFFD_FEATURE_PAGEFAULT_FLAG_WP, &api) != 0) {
        close(fd);
        return 1;
    }

    long page_size = sysconf(_SC_PAGESIZE);
    if (page_size <= 0) {
        fprintf(stderr, "sysconf(_SC_PAGESIZE) failed\n");
        close(fd);
        return 1;
    }

    void *page = mmap(NULL, (size_t)page_size, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (page == MAP_FAILED) {
        fprintf(stderr, "mmap failed: errno=%d (%s)\n", errno, strerror(errno));
        close(fd);
        return 1;
    }

    struct uffdio_register reg = {
        .range = {
            .start = (unsigned long)page,
            .len = (unsigned long)page_size,
        },
        .mode = UFFDIO_REGISTER_MODE_WP,
    };
    if (ioctl(fd, UFFDIO_REGISTER, &reg) != 0) {
        fprintf(stderr, "UFFDIO_REGISTER_MODE_WP failed: errno=%d (%s)\n", errno, strerror(errno));
        munmap(page, (size_t)page_size);
        close(fd);
        return 1;
    }
    if ((reg.ioctls & (1ULL << _UFFDIO_WRITEPROTECT)) == 0) {
        fprintf(stderr, "UFFDIO_REGISTER did not report UFFDIO_WRITEPROTECT\n");
        munmap(page, (size_t)page_size);
        close(fd);
        return 1;
    }

    struct uffdio_writeprotect wp = {
        .range = {
            .start = (unsigned long)page,
            .len = (unsigned long)page_size,
        },
        .mode = UFFDIO_WRITEPROTECT_MODE_WP,
    };
    if (ioctl(fd, UFFDIO_WRITEPROTECT, &wp) != 0) {
        fprintf(stderr, "UFFDIO_WRITEPROTECT enable failed: errno=%d (%s)\n", errno, strerror(errno));
        munmap(page, (size_t)page_size);
        close(fd);
        return 1;
    }

    wp.mode = 0;
    if (ioctl(fd, UFFDIO_WRITEPROTECT, &wp) != 0) {
        fprintf(stderr, "UFFDIO_WRITEPROTECT disable failed: errno=%d (%s)\n", errno, strerror(errno));
        munmap(page, (size_t)page_size);
        close(fd);
        return 1;
    }

    struct uffdio_range unreg = {
        .start = (unsigned long)page,
        .len = (unsigned long)page_size,
    };
    if (ioctl(fd, UFFDIO_UNREGISTER, &unreg) != 0) {
        fprintf(stderr, "UFFDIO_UNREGISTER failed: errno=%d (%s)\n", errno, strerror(errno));
        munmap(page, (size_t)page_size);
        close(fd);
        return 1;
    }

    munmap(page, (size_t)page_size);
    close(fd);
    puts("uffd sync write-protect ioctl supported");
    return 0;
}

int main(void) {
    struct utsname uts;
    if (uname(&uts) == 0) {
        fprintf(stderr, "kernel: %s %s %s\n", uts.sysname, uts.release, uts.machine);
    }

    int fd = open_uffd();
    if (fd < 0) {
        return 1;
    }

    unsigned long long requested =
        UFFD_FEATURE_PAGEFAULT_FLAG_WP |
        UFFD_FEATURE_EVENT_REMOVE |
        UFFD_FEATURE_MISSING_HUGETLBFS |
        UFFD_FEATURE_MISSING_SHMEM |
        UFFD_FEATURE_MINOR_HUGETLBFS |
        UFFD_FEATURE_MINOR_SHMEM |
        UFFD_FEATURE_WP_HUGETLBFS_SHMEM |
        UFFD_FEATURE_WP_UNPOPULATED |
        UFFD_FEATURE_WP_ASYNC;
    struct uffdio_api api;
    if (enable_api(fd, requested, &api) != 0) {
        close(fd);
        return 1;
    }

    printf("features=0x%llx\n", (unsigned long long)api.features);
    if (require_features(api.features) != 0) {
        close(fd);
        return 1;
    }

    close(fd);
    puts("uffd missing/minor/wp optional features supported");
    return probe_sync_wp();
}
