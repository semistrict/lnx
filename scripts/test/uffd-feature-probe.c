#include <errno.h>
#include <fcntl.h>
#include <linux/userfaultfd.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
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

int main(void) {
    struct utsname uts;
    if (uname(&uts) == 0) {
        fprintf(stderr, "kernel: %s %s %s\n", uts.sysname, uts.release, uts.machine);
    }

    int fd = (int)syscall(SYS_userfaultfd, O_CLOEXEC | UFFD_USER_MODE_ONLY);
    if (fd < 0) {
        fprintf(stderr, "userfaultfd failed: errno=%d (%s)\n", errno, strerror(errno));
        return 1;
    }

    struct uffdio_api api = {
        .api = UFFD_API,
        .features = UFFD_FEATURE_MINOR_SHMEM | UFFD_FEATURE_MINOR_HUGETLBFS,
    };
    if (ioctl(fd, UFFDIO_API, &api) != 0) {
        fprintf(stderr, "UFFDIO_API failed: errno=%d (%s)\n", errno, strerror(errno));
        close(fd);
        return 1;
    }

    printf("features=0x%llx\n", (unsigned long long)api.features);
    if ((api.features & UFFD_FEATURE_MINOR_SHMEM) == 0) {
        fprintf(stderr, "missing UFFD_FEATURE_MINOR_SHMEM\n");
        close(fd);
        return 1;
    }
    if ((api.features & UFFD_FEATURE_MINOR_HUGETLBFS) == 0) {
        fprintf(stderr, "missing UFFD_FEATURE_MINOR_HUGETLBFS\n");
        close(fd);
        return 1;
    }

    close(fd);
    puts("uffd minor shmem and hugetlbfs supported");
    return 0;
}
