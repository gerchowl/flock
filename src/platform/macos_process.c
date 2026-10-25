// Cross-UID process facts. proc_pidinfo denies root-owned ancestors to users.
// Compile against the SDK so both Darwin architectures use its kinfo_proc ABI.
#include <stdint.h>
#include <sys/types.h>
#include <sys/sysctl.h>
#include <sys/proc.h>

int flock_process_facts(int pid, uint32_t *parent, uint64_t *birth, int live_only) {
    int mib[] = { CTL_KERN, KERN_PROC, KERN_PROC_PID, pid };
    struct kinfo_proc info = {0};
    size_t size = sizeof(info);
    if (pid <= 0 || sysctl(mib, 4, &info, &size, NULL, 0) != 0 ||
        size != sizeof(info) || info.kp_proc.p_pid != pid ||
        (live_only && info.kp_proc.p_stat == SZOMB) ||
        info.kp_eproc.e_ppid < 0 || info.kp_proc.p_starttime.tv_sec <= 0)
        return 0;
    *parent = (uint32_t)info.kp_eproc.e_ppid;
    *birth = (uint64_t)info.kp_proc.p_starttime.tv_sec * 1000000 +
             (uint64_t)info.kp_proc.p_starttime.tv_usec;
    return 1;
}
