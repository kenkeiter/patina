/*
 * Kernel-internal ABI definitions that are not exported to the UAPI headers,
 * transcribed from Linux v7.2.10 source (file:line given for each). Member
 * types are UAPI types, so layouts are still computed by the compiler; only
 * the declarations are hand-copied. Re-check these when bumping the kernel
 * version in extract.sh.
 */
#ifndef PATINA_KINTERNAL_H
#define PATINA_KINTERNAL_H

typedef unsigned long long u64;
typedef long long s64;

/* arch/x86/include/asm/signal.h:11 (the UAPI x86 header only has the libc
 * NSIG 32; asm-generic/signal.h exports _NSIG 64 for arm64). */
#ifndef _NSIG
#define _NSIG 64
#endif

/* include/linux/signal_types.h:37 struct sigaction (with __ARCH_HAS_SA_RESTORER,
 * which both x86_64 and arm64 define). This, not the UAPI x86 `struct
 * sigaction`, is what rt_sigaction copies (kernel/signal.c). */
struct k_sigaction_abi {
	__sighandler_t sa_handler;
	unsigned long sa_flags;
	__sigrestore_t sa_restorer;
	sigset_t sa_mask;
};

/* include/linux/dirent.h:5 */
struct linux_dirent64 {
	u64 d_ino;
	s64 d_off;
	unsigned short d_reclen;
	unsigned char d_type;
	char d_name[];
};

/* include/linux/socket.h:58 */
struct linger {
	int l_onoff;
	int l_linger;
};

/* include/linux/socket.h:97 */
struct user_msghdr {
	void *msg_name;
	int msg_namelen;
	struct iovec *msg_iov;
	__kernel_size_t msg_iovlen;
	void *msg_control;
	__kernel_size_t msg_controllen;
	unsigned int msg_flags;
};

/* include/linux/socket.h:119 */
struct cmsghdr {
	__kernel_size_t cmsg_len;
	int cmsg_level;
	int cmsg_type;
};
/* include/linux/socket.h:133-140 */
#define CMSG_ALIGN(len) (((len) + sizeof(long) - 1) & ~(sizeof(long) - 1))
#define CMSG_SPACE(len) (sizeof(struct cmsghdr) + CMSG_ALIGN(len))
#define CMSG_LEN(len) (sizeof(struct cmsghdr) + (len))

/* include/linux/socket.h:197 */
struct ucred {
	__u32 pid;
	__u32 uid;
	__u32 gid;
};

/* include/linux/socket.h:192-193 */
#define SCM_RIGHTS 0x01
#define SCM_CREDENTIALS 0x02
/* include/linux/socket.h:204-246 */
#define AF_UNSPEC 0
#define AF_UNIX 1
#define AF_LOCAL 1
#define AF_INET 2
#define AF_INET6 10
#define AF_NETLINK 16
#define AF_PACKET 17
#define AF_VSOCK 40
/* include/linux/socket.h:312 */
#define SOMAXCONN 4096
/* include/linux/socket.h:318-348 */
#define MSG_OOB 1
#define MSG_PEEK 2
#define MSG_DONTROUTE 4
#define MSG_CTRUNC 8
#define MSG_PROBE 0x10
#define MSG_TRUNC 0x20
#define MSG_DONTWAIT 0x40
#define MSG_EOR 0x80
#define MSG_WAITALL 0x100
#define MSG_FIN 0x200
#define MSG_SYN 0x400
#define MSG_CONFIRM 0x800
#define MSG_RST 0x1000
#define MSG_ERRQUEUE 0x2000
#define MSG_NOSIGNAL 0x4000
#define MSG_MORE 0x8000
#define MSG_WAITFORONE 0x10000
#define MSG_FASTOPEN 0x20000000
#define MSG_CMSG_CLOEXEC 0x40000000
/* include/linux/socket.h:362-366 (SOL_SOCKET is UAPI asm/socket.h) */
#define SOL_IP 0
#define SOL_TCP 6
#define SOL_UDP 17
#define SOL_IPV6 41

/* include/linux/net.h:85-103 */
#define SOCK_STREAM 1
#define SOCK_DGRAM 2
#define SOCK_RAW 3
#define SOCK_RDM 4
#define SOCK_SEQPACKET 5
#define SOCK_TYPE_MASK 0xf
#define SOCK_CLOEXEC O_CLOEXEC
#define SOCK_NONBLOCK O_NONBLOCK

/* shutdown(2) `how`: net/ipv4/af_inet.c:907 maps how 0/1/2 to
 * RCV_SHUTDOWN/SEND_SHUTDOWN/both via how++ (no named kernel constant). */
#define SHUT_RD 0
#define SHUT_WR 1
#define SHUT_RDWR 2

/* include/linux/stat.h:15-16 */
#define UTIME_NOW ((1l << 30) - 1l)
#define UTIME_OMIT ((1l << 30) - 2l)

/* include/linux/sched.h:325 */
#define TASK_COMM_LEN 16

/* include/linux/posix-timers_types.h:20-31 */
#define CPUCLOCK_PERTHREAD_MASK 4
#define CPUCLOCK_CLOCK_MASK 3
#define CPUCLOCK_PROF 0
#define CPUCLOCK_VIRT 1
#define CPUCLOCK_SCHED 2
#define CPUCLOCK_MAX 3
/* include/linux/posix-timers.h:19-28 (static inlines there; macros here so
 * the result is an integer constant expression). */
#define make_process_cpuclock(pid, clock) \
	((__kernel_clockid_t)((~(unsigned int)(pid)) << 3) | (clock))
#define make_thread_cpuclock(tid, clock) \
	make_process_cpuclock((tid), (clock) | CPUCLOCK_PERTHREAD_MASK)

#if defined(__x86_64__)

/* arch/x86/include/asm/sigframe.h:59 (CONFIG_X86_64). `struct ucontext` is
 * the UAPI one (asm-generic/ucontext.h via asm/ucontext.h), `struct siginfo`
 * is the UAPI asm-generic/siginfo.h one. */
struct rt_sigframe {
	char *pretcode;
	struct ucontext uc;
	struct siginfo info;
	/* fp state follows here */
};
/* arch/x86/kernel/signal.c:86 */
#define FRAME_ALIGNMENT 16
/* arch/x86/kernel/signal.c:107-109: 128-byte red zone skipped on the
 * current stack (not on a freshly entered sigaltstack). */
#define REDZONE 128
/* arch/x86/kernel/fpu/signal.c:500 round_down(sp - frame_size, 64) */
#define FPSTATE_ALIGN 64

#else /* aarch64 */

/* arch/arm64/kernel/signal.c:45 */
struct rt_sigframe {
	struct siginfo info;
	struct ucontext uc;
};
/* arch/arm64/include/asm/stacktrace/frame.h:32 */
struct frame_record {
	u64 fp;
	u64 lr;
};

/* arch/arm64/kernel/signal.c:70-71 */
#define ROUND_UP16(x) (((x) + 15) & ~(unsigned long)15)
#define TERMINATOR_SIZE ROUND_UP16(sizeof(struct _aarch64_ctx))
#define EXTRA_CONTEXT_SIZE ROUND_UP16(sizeof(struct extra_context))

/*
 * Record placement for a task with only FPSIMD state (no SVE/SME/GCS/
 * POE/FPMR): setup_sigframe_layout() (signal.c:1178) allocates, relative
 * to the start of rt_sigframe and starting at
 * offsetof(rt_sigframe, uc.uc_mcontext.__reserved) (init_user_layout,
 * signal.c:162): fpsimd_context, then esr_context iff
 * thread.fault_code != 0 (signal.c:1191), then the terminator
 * (sigframe_alloc_end, signal.c:234). Each allocation is rounded up to 16
 * (__sigframe_alloc, signal.c:187). The frame size is
 * round_up(max(user->size, sizeof(rt_sigframe)), 16) (signal.c:173).
 */
#define SIGFRAME_RESERVED_BASE __builtin_offsetof(struct rt_sigframe, uc.uc_mcontext.__reserved)
#define SIGFRAME_FPSIMD_OFFSET SIGFRAME_RESERVED_BASE
#define SIGFRAME_ESR_OFFSET (SIGFRAME_FPSIMD_OFFSET + ROUND_UP16(sizeof(struct fpsimd_context)))
#define SIGFRAME_END_OFFSET (SIGFRAME_ESR_OFFSET + ROUND_UP16(sizeof(struct esr_context)))
#define SIGFRAME_END_OFFSET_NOESR SIGFRAME_ESR_OFFSET
#define SIGFRAME_USER_SIZE (SIGFRAME_END_OFFSET + TERMINATOR_SIZE)
#define SIGFRAME_SIZE ROUND_UP16(SIGFRAME_USER_SIZE > sizeof(struct rt_sigframe) ? \
				 SIGFRAME_USER_SIZE : sizeof(struct rt_sigframe))

#endif

#endif
