/* Test-only interposer for probe_durable_attach.py. No production linkage. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>
#include <errno.h>
#ifdef __APPLE__
#include <pthread.h>
#include <stdarg.h>
#else
#include <sys/syscall.h>
#endif

static void delay_sync(int fd) {
    const char *target = getenv("PROBE_SYNC_TARGET");
    const char *arm = getenv("PROBE_SYNC_ARM");
    const char *entered = getenv("PROBE_SYNC_ENTERED");
    struct stat actual, wanted;
    int saved = errno;
#ifdef __APPLE__
    int main_thread = pthread_main_np();
#else
    int main_thread = syscall(SYS_gettid) == getpid();
#endif
    if (target && arm && entered && !fstat(fd, &actual) && !stat(target, &wanted)
        && actual.st_dev == wanted.st_dev && actual.st_ino == wanted.st_ino
        && !unlink(arm)) {
        int marker = open(entered, O_WRONLY | O_CREAT | O_EXCL, 0600);
        if (marker >= 0) {
            const char *thread = main_thread ? "main" : "worker";
            (void)write(marker, thread, main_thread ? 4 : 6);
            close(marker);
        }
        struct timespec delay = { .tv_sec = 8, .tv_nsec = 0 };
        while (nanosleep(&delay, &delay) && errno == EINTR) {}
    }
    errno = saved;
}

#ifdef __APPLE__
static int probe_fsync(int fd) {
    delay_sync(fd);
    return fsync(fd);
}
static int probe_fcntl(int fd, int command, ...) {
    if (command == F_FULLFSYNC || command == F_BARRIERFSYNC) {
        delay_sync(fd);
        return fcntl(fd, command);
    }
    if (command == F_GETFD || command == F_GETFL || command == F_GETOWN) {
        return fcntl(fd, command);
    }
    va_list args;
    va_start(args, command);
    int result;
    switch (command) {
        case F_GETLK: case F_SETLK: case F_SETLKW: case F_GETPATH:
        case F_PREALLOCATE: case F_RDADVISE:
            result = fcntl(fd, command, va_arg(args, void *));
            break;
        default:
            result = fcntl(fd, command, va_arg(args, int));
            break;
    }
    va_end(args);
    return result;
}
__attribute__((used)) static struct { const void *replacement, *original; }
    interpose_fsync __attribute__((section("__DATA,__interpose"))) = {
        (const void *)probe_fsync, (const void *)fsync
    };
__attribute__((used)) static struct { const void *replacement, *original; }
    interpose_fcntl __attribute__((section("__DATA,__interpose"))) = {
        (const void *)probe_fcntl, (const void *)fcntl
    };
#else
int fsync(int fd) {
    int (*real_sync)(int) = dlsym(RTLD_NEXT, "fsync");
    delay_sync(fd);
    return real_sync(fd);
}
int fdatasync(int fd) {
    int (*real_sync)(int) = dlsym(RTLD_NEXT, "fdatasync");
    delay_sync(fd);
    return real_sync(fd);
}
#endif
