/*
 * ABI value extractor for patina-abi (see README.md / extract.sh).
 *
 * Compiled with `clang --target={x86_64,aarch64}-linux-gnu -nostdinc -S`
 * against sanitized Linux UAPI headers only. Nothing is linked or run: every
 * value is emitted as an immediate operand of an inline-asm `.ascii`
 * directive ("@@ key value") and scraped from the assembly by extract.sh.
 *
 * Keys:
 *   <struct>.__size / <struct>.__align   sizeof / _Alignof
 *   <struct>.<field>                     offsetof
 *   <MACRO>                              value of a UAPI constant
 *   nr.<name>, errno.<NAME>              appended by extract.sh (generated)
 *
 * Structures that are kernel-internal (not exported to UAPI) are reproduced
 * in kinternal.h from the kernel source named there, built from UAPI member
 * types, so their layout is still computed by the compiler.
 */

#include <linux/types.h>
#include <linux/version.h>
#include <linux/errno.h>
#include <asm/unistd.h>
#include <linux/time.h>
#include <linux/time_types.h>
#include <linux/signal.h>
#include <asm/sigcontext.h>
#include <asm/ucontext.h>
#include <asm/stat.h>
#include <linux/stat.h>
#include <linux/fcntl.h>
#include <linux/fs.h>
#include <linux/uio.h>
#include <linux/poll.h>
#include <linux/eventpoll.h>
#include <linux/eventfd.h>
#include <linux/random.h>
#include <linux/socket.h>
#include <asm/socket.h>
#include <linux/in.h>
#include <linux/in6.h>
#include <linux/un.h>
#include <linux/vm_sockets.h>
#include <linux/tcp.h>
#include <linux/resource.h>
#include <linux/utsname.h>
#include <linux/times.h>
#include <asm/termbits.h>
#include <asm/ioctls.h>
#include <asm/termios.h>
#include <linux/prctl.h>
#include <linux/membarrier.h>
#include <linux/sched.h>
#include <linux/futex.h>
#include <linux/wait.h>
#include <linux/mman.h>
#include <linux/auxvec.h>
#include <asm/auxvec.h>
#if defined(__x86_64__)
#include <asm/prctl.h>
#else
#include <asm/hwcap.h>
#endif

#include "kinternal.h"

#define V(key, val) \
	__asm__ volatile("\n.ascii \"@@ " key " %c0\\n\"" : : "i"((long long)(val)))
/* Constant: key is the macro name. */
#define C(m) V(#m, m)
/* Struct size + alignment. */
#define S(key, T) do { V(key ".__size", sizeof(T)); V(key ".__align", _Alignof(T)); } while (0)
/* Field offset (nested designators allowed: a.b.c). */
#define O(key, T, f) V(key "." #f, __builtin_offsetof(T, f))
/* Field size. */
#define Z(key, T, f) V(key "." #f ".__size", sizeof(((T *)0)->f))

void common(void)
{
	C(LINUX_VERSION_MAJOR); C(LINUX_VERSION_PATCHLEVEL); C(LINUX_VERSION_SUBLEVEL);

	/* ---- time ---- */
	S("timespec", struct __kernel_timespec);
	O("timespec", struct __kernel_timespec, tv_sec);
	O("timespec", struct __kernel_timespec, tv_nsec);
	S("timeval", struct __kernel_old_timeval);
	O("timeval", struct __kernel_old_timeval, tv_sec);
	O("timeval", struct __kernel_old_timeval, tv_usec);
	S("itimerval", struct itimerval);
	O("itimerval", struct itimerval, it_interval);
	O("itimerval", struct itimerval, it_value);
	S("itimerspec", struct __kernel_itimerspec);
	O("itimerspec", struct __kernel_itimerspec, it_interval);
	O("itimerspec", struct __kernel_itimerspec, it_value);
	S("timezone", struct timezone);
	O("timezone", struct timezone, tz_minuteswest);
	O("timezone", struct timezone, tz_dsttime);
	C(CLOCK_REALTIME); C(CLOCK_MONOTONIC); C(CLOCK_PROCESS_CPUTIME_ID);
	C(CLOCK_THREAD_CPUTIME_ID); C(CLOCK_MONOTONIC_RAW); C(CLOCK_REALTIME_COARSE);
	C(CLOCK_MONOTONIC_COARSE); C(CLOCK_BOOTTIME); C(CLOCK_REALTIME_ALARM);
	C(CLOCK_BOOTTIME_ALARM); C(CLOCK_TAI);
	C(TIMER_ABSTIME);
	C(ITIMER_REAL); C(ITIMER_VIRTUAL); C(ITIMER_PROF);
	C(CPUCLOCK_PROF); C(CPUCLOCK_VIRT); C(CPUCLOCK_SCHED);
	C(CPUCLOCK_PERTHREAD_MASK); C(CPUCLOCK_CLOCK_MASK); C(CPUCLOCK_MAX);
	V("make_process_cpuclock(0,CPUCLOCK_SCHED)", make_process_cpuclock(0, CPUCLOCK_SCHED));
	V("make_thread_cpuclock(1234,CPUCLOCK_SCHED)", make_thread_cpuclock(1234, CPUCLOCK_SCHED));
	V("make_process_cpuclock(1,CPUCLOCK_PROF)", make_process_cpuclock(1, CPUCLOCK_PROF));
	C(UTIME_NOW); C(UTIME_OMIT);

	/* ---- signals ---- */
	C(SIGHUP); C(SIGINT); C(SIGQUIT); C(SIGILL); C(SIGTRAP); C(SIGABRT); C(SIGIOT);
	C(SIGBUS); C(SIGFPE); C(SIGKILL); C(SIGUSR1); C(SIGSEGV); C(SIGUSR2); C(SIGPIPE);
	C(SIGALRM); C(SIGTERM); C(SIGSTKFLT); C(SIGCHLD); C(SIGCONT); C(SIGSTOP); C(SIGTSTP);
	C(SIGTTIN); C(SIGTTOU); C(SIGURG); C(SIGXCPU); C(SIGXFSZ); C(SIGVTALRM); C(SIGPROF);
	C(SIGWINCH); C(SIGIO); C(SIGPOLL); C(SIGPWR); C(SIGSYS);
	C(SIGRTMIN); C(SIGRTMAX); C(_NSIG);
	C(SIG_BLOCK); C(SIG_UNBLOCK); C(SIG_SETMASK);
	C(SIG_DFL); C(SIG_IGN); C(SIG_ERR);
	C(SA_NOCLDSTOP); C(SA_NOCLDWAIT); C(SA_SIGINFO); C(SA_UNSUPPORTED);
	C(SA_EXPOSE_TAGBITS); C(SA_RESTORER); C(SA_ONSTACK); C(SA_RESTART);
	C(SA_NODEFER); C(SA_RESETHAND);
	C(SS_ONSTACK); C(SS_DISABLE); C(SS_AUTODISARM);
	C(MINSIGSTKSZ); C(SIGSTKSZ);
	C(SI_USER); C(SI_KERNEL); C(SI_QUEUE); C(SI_TIMER); C(SI_MESGQ); C(SI_ASYNCIO);
	C(SI_SIGIO); C(SI_TKILL);
	C(SEGV_MAPERR); C(SEGV_ACCERR); C(SEGV_BNDERR); C(SEGV_PKUERR);
	C(BUS_ADRALN); C(BUS_ADRERR); C(BUS_OBJERR);
	C(ILL_ILLOPC); C(ILL_ILLOPN); C(ILL_ILLADR); C(ILL_ILLTRP); C(ILL_PRVOPC);
	C(ILL_PRVREG); C(ILL_COPROC); C(ILL_BADSTK);
	C(FPE_INTDIV); C(FPE_INTOVF); C(FPE_FLTDIV); C(FPE_FLTOVF); C(FPE_FLTUND);
	C(FPE_FLTRES); C(FPE_FLTINV); C(FPE_FLTSUB);
	C(TRAP_BRKPT); C(TRAP_TRACE);
	C(CLD_EXITED); C(CLD_KILLED); C(CLD_DUMPED);
	C(SIGEV_SIGNAL); C(SIGEV_NONE); C(SIGEV_THREAD); C(SIGEV_THREAD_ID);
	V("sigset_t.__size", sizeof(sigset_t));

	/* Kernel struct sigaction as used by rt_sigaction (kinternal.h). */
	S("sigaction", struct k_sigaction_abi);
	O("sigaction", struct k_sigaction_abi, sa_handler);
	O("sigaction", struct k_sigaction_abi, sa_flags);
	O("sigaction", struct k_sigaction_abi, sa_restorer);
	O("sigaction", struct k_sigaction_abi, sa_mask);

	S("stack_t", stack_t);
	O("stack_t", stack_t, ss_sp);
	O("stack_t", stack_t, ss_flags);
	O("stack_t", stack_t, ss_size);

	S("siginfo", siginfo_t);
	O("siginfo", siginfo_t, si_signo);
	O("siginfo", siginfo_t, si_errno);
	O("siginfo", siginfo_t, si_code);
	O("siginfo", siginfo_t, _sifields);
	O("siginfo", siginfo_t, si_pid);
	O("siginfo", siginfo_t, si_uid);
	O("siginfo", siginfo_t, si_tid);
	O("siginfo", siginfo_t, si_overrun);
	O("siginfo", siginfo_t, si_value);
	O("siginfo", siginfo_t, si_status);
	O("siginfo", siginfo_t, si_utime);
	O("siginfo", siginfo_t, si_stime);
	O("siginfo", siginfo_t, si_addr);
	O("siginfo", siginfo_t, si_band);
	O("siginfo", siginfo_t, si_fd);

	S("sigevent", sigevent_t);
	O("sigevent", sigevent_t, sigev_value);
	O("sigevent", sigevent_t, sigev_signo);
	O("sigevent", sigevent_t, sigev_notify);
	O("sigevent", sigevent_t, _sigev_un);
	O("sigevent", sigevent_t, sigev_notify_thread_id);

	/* ---- fs ---- */
	C(O_RDONLY); C(O_WRONLY); C(O_RDWR); C(O_ACCMODE); C(O_CREAT); C(O_EXCL);
	C(O_NOCTTY); C(O_TRUNC); C(O_APPEND); C(O_NONBLOCK); C(O_NDELAY); C(O_DSYNC);
	C(FASYNC); C(O_NOATIME); C(O_CLOEXEC); C(__O_SYNC); C(O_SYNC); C(O_PATH);
	C(__O_TMPFILE); C(O_TMPFILE); C(O_DIRECT); C(O_LARGEFILE); C(O_DIRECTORY);
	C(O_NOFOLLOW);
	C(AT_FDCWD); C(AT_SYMLINK_NOFOLLOW); C(AT_REMOVEDIR); C(AT_EACCESS);
	C(AT_SYMLINK_FOLLOW); C(AT_NO_AUTOMOUNT); C(AT_EMPTY_PATH);
	C(AT_STATX_SYNC_TYPE); C(AT_STATX_SYNC_AS_STAT); C(AT_STATX_FORCE_SYNC);
	C(AT_STATX_DONT_SYNC);
	C(RENAME_NOREPLACE); C(RENAME_EXCHANGE); C(RENAME_WHITEOUT);
	C(SEEK_SET); C(SEEK_CUR); C(SEEK_END); C(SEEK_DATA); C(SEEK_HOLE);
	C(F_DUPFD); C(F_GETFD); C(F_SETFD); C(F_GETFL); C(F_SETFL); C(F_GETLK);
	C(F_SETLK); C(F_SETLKW); C(F_SETOWN); C(F_GETOWN); C(F_SETSIG); C(F_GETSIG);
	C(F_SETOWN_EX); C(F_GETOWN_EX); C(F_OFD_GETLK); C(F_OFD_SETLK); C(F_OFD_SETLKW);
	C(F_SETLEASE); C(F_GETLEASE); C(F_NOTIFY); C(F_DUPFD_CLOEXEC); C(F_SETPIPE_SZ);
	C(F_GETPIPE_SZ); C(F_ADD_SEALS); C(F_GET_SEALS); C(FD_CLOEXEC);
	C(F_RDLCK); C(F_WRLCK); C(F_UNLCK);
	S("flock", struct flock);
	O("flock", struct flock, l_type);
	O("flock", struct flock, l_whence);
	O("flock", struct flock, l_start);
	O("flock", struct flock, l_len);
	O("flock", struct flock, l_pid);
	C(S_IFMT); C(S_IFSOCK); C(S_IFLNK); C(S_IFREG); C(S_IFBLK); C(S_IFDIR);
	C(S_IFCHR); C(S_IFIFO); C(S_ISUID); C(S_ISGID); C(S_ISVTX);
	C(STATX_TYPE); C(STATX_MODE); C(STATX_NLINK); C(STATX_UID); C(STATX_GID);
	C(STATX_ATIME); C(STATX_MTIME); C(STATX_CTIME); C(STATX_INO); C(STATX_SIZE);
	C(STATX_BLOCKS); C(STATX_BASIC_STATS); C(STATX_BTIME); C(STATX_MNT_ID);
	C(STATX_DIOALIGN); C(STATX_MNT_ID_UNIQUE); C(STATX_SUBVOL);
	C(STATX_WRITE_ATOMIC); C(STATX_DIO_READ_ALIGN);
	S("statx_timestamp", struct statx_timestamp);
	O("statx_timestamp", struct statx_timestamp, tv_sec);
	O("statx_timestamp", struct statx_timestamp, tv_nsec);
	O("statx_timestamp", struct statx_timestamp, __reserved);
	S("statx", struct statx);
	O("statx", struct statx, stx_mask);
	O("statx", struct statx, stx_blksize);
	O("statx", struct statx, stx_attributes);
	O("statx", struct statx, stx_nlink);
	O("statx", struct statx, stx_uid);
	O("statx", struct statx, stx_gid);
	O("statx", struct statx, stx_mode);
	O("statx", struct statx, __spare0);
	O("statx", struct statx, stx_ino);
	O("statx", struct statx, stx_size);
	O("statx", struct statx, stx_blocks);
	O("statx", struct statx, stx_attributes_mask);
	O("statx", struct statx, stx_atime);
	O("statx", struct statx, stx_btime);
	O("statx", struct statx, stx_ctime);
	O("statx", struct statx, stx_mtime);
	O("statx", struct statx, stx_rdev_major);
	O("statx", struct statx, stx_rdev_minor);
	O("statx", struct statx, stx_dev_major);
	O("statx", struct statx, stx_dev_minor);
	O("statx", struct statx, stx_mnt_id);
	O("statx", struct statx, stx_dio_mem_align);
	O("statx", struct statx, stx_dio_offset_align);
	O("statx", struct statx, stx_subvol);
	O("statx", struct statx, stx_atomic_write_unit_min);
	O("statx", struct statx, stx_atomic_write_unit_max);
	O("statx", struct statx, stx_atomic_write_segments_max);
	O("statx", struct statx, stx_dio_read_offset_align);
	O("statx", struct statx, stx_atomic_write_unit_max_opt);
	O("statx", struct statx, __spare2);
	O("statx", struct statx, __spare3);
	S("iovec", struct iovec);
	O("iovec", struct iovec, iov_base);
	O("iovec", struct iovec, iov_len);
	C(UIO_MAXIOV);
	S("pollfd", struct pollfd);
	O("pollfd", struct pollfd, fd);
	O("pollfd", struct pollfd, events);
	O("pollfd", struct pollfd, revents);
	C(POLLIN); C(POLLPRI); C(POLLOUT); C(POLLERR); C(POLLHUP); C(POLLNVAL);
	C(POLLRDNORM); C(POLLRDBAND); C(POLLWRNORM); C(POLLWRBAND); C(POLLMSG);
	C(POLLRDHUP);
	S("linux_dirent64", struct linux_dirent64);
	O("linux_dirent64", struct linux_dirent64, d_ino);
	O("linux_dirent64", struct linux_dirent64, d_off);
	O("linux_dirent64", struct linux_dirent64, d_reclen);
	O("linux_dirent64", struct linux_dirent64, d_type);
	O("linux_dirent64", struct linux_dirent64, d_name);

	/* ---- mm ---- */
	C(PROT_NONE); C(PROT_READ); C(PROT_WRITE); C(PROT_EXEC); C(PROT_SEM);
	C(PROT_GROWSDOWN); C(PROT_GROWSUP);
	C(MAP_SHARED); C(MAP_PRIVATE); C(MAP_SHARED_VALIDATE); C(MAP_TYPE); C(MAP_FIXED);
	C(MAP_ANONYMOUS); C(MAP_GROWSDOWN); C(MAP_DENYWRITE); C(MAP_EXECUTABLE);
	C(MAP_LOCKED); C(MAP_NORESERVE); C(MAP_POPULATE); C(MAP_NONBLOCK); C(MAP_STACK);
	C(MAP_HUGETLB); C(MAP_SYNC); C(MAP_FIXED_NOREPLACE); C(MAP_UNINITIALIZED);
	C(MAP_HUGE_SHIFT);
	C(MADV_NORMAL); C(MADV_RANDOM); C(MADV_SEQUENTIAL); C(MADV_WILLNEED);
	C(MADV_DONTNEED); C(MADV_FREE); C(MADV_REMOVE); C(MADV_DONTFORK); C(MADV_DOFORK);
	C(MADV_MERGEABLE); C(MADV_UNMERGEABLE); C(MADV_HUGEPAGE); C(MADV_NOHUGEPAGE);
	C(MADV_DONTDUMP); C(MADV_DODUMP); C(MADV_WIPEONFORK); C(MADV_KEEPONFORK);
	C(MADV_COLD); C(MADV_PAGEOUT); C(MADV_POPULATE_READ); C(MADV_POPULATE_WRITE);
	C(MADV_DONTNEED_LOCKED); C(MADV_COLLAPSE); C(MADV_HWPOISON); C(MADV_SOFT_OFFLINE);
	C(MREMAP_MAYMOVE); C(MREMAP_FIXED); C(MREMAP_DONTUNMAP);
	C(MS_ASYNC); C(MS_INVALIDATE); C(MS_SYNC);

	/* ---- sched / futex / wait ---- */
	C(CSIGNAL); C(CLONE_VM); C(CLONE_FS); C(CLONE_FILES); C(CLONE_SIGHAND);
	C(CLONE_PIDFD); C(CLONE_PTRACE); C(CLONE_VFORK); C(CLONE_PARENT); C(CLONE_THREAD);
	C(CLONE_NEWNS); C(CLONE_SYSVSEM); C(CLONE_SETTLS); C(CLONE_PARENT_SETTID);
	C(CLONE_CHILD_CLEARTID); C(CLONE_DETACHED); C(CLONE_UNTRACED);
	C(CLONE_CHILD_SETTID); C(CLONE_NEWCGROUP); C(CLONE_NEWUTS); C(CLONE_NEWIPC);
	C(CLONE_NEWUSER); C(CLONE_NEWPID); C(CLONE_NEWNET); C(CLONE_IO);
	C(SCHED_NORMAL); C(SCHED_FIFO); C(SCHED_RR); C(SCHED_BATCH); C(SCHED_IDLE);
	C(FUTEX_WAIT); C(FUTEX_WAKE); C(FUTEX_FD); C(FUTEX_REQUEUE); C(FUTEX_CMP_REQUEUE);
	C(FUTEX_WAKE_OP); C(FUTEX_LOCK_PI); C(FUTEX_UNLOCK_PI); C(FUTEX_TRYLOCK_PI);
	C(FUTEX_WAIT_BITSET); C(FUTEX_WAKE_BITSET); C(FUTEX_WAIT_REQUEUE_PI);
	C(FUTEX_CMP_REQUEUE_PI); C(FUTEX_LOCK_PI2); C(FUTEX_PRIVATE_FLAG);
	C(FUTEX_CLOCK_REALTIME); C(FUTEX_ROBUST_UNLOCK); C(FUTEX_ROBUST_LIST32);
	C(FUTEX_CMD_MASK); C(FUTEX_WAIT_PRIVATE);
	C(FUTEX_WAKE_PRIVATE); C(FUTEX_BITSET_MATCH_ANY);
	C(WNOHANG); C(WUNTRACED); C(WEXITED); C(WCONTINUED); C(WNOWAIT);
	C(__WALL); C(__WCLONE);

	/* ---- epoll / eventfd / getrandom ---- */
	C(EPOLL_CTL_ADD); C(EPOLL_CTL_DEL); C(EPOLL_CTL_MOD); C(EPOLL_CLOEXEC);
	C(EPOLLIN); C(EPOLLPRI); C(EPOLLOUT); C(EPOLLERR); C(EPOLLHUP); C(EPOLLNVAL);
	C(EPOLLRDNORM); C(EPOLLRDBAND); C(EPOLLWRNORM); C(EPOLLWRBAND); C(EPOLLMSG);
	C(EPOLLRDHUP); C(EPOLLEXCLUSIVE); C(EPOLLWAKEUP); C(EPOLLONESHOT); C(EPOLLET);
	S("epoll_event", struct epoll_event);
	O("epoll_event", struct epoll_event, events);
	O("epoll_event", struct epoll_event, data);
	C(EFD_SEMAPHORE); C(EFD_CLOEXEC); C(EFD_NONBLOCK);
	C(GRND_NONBLOCK); C(GRND_RANDOM); C(GRND_INSECURE);

	/* ---- net ---- */
	C(AF_UNSPEC); C(AF_UNIX); C(AF_LOCAL); C(AF_INET); C(AF_INET6); C(AF_NETLINK);
	C(AF_PACKET); C(AF_VSOCK);
	C(SOCK_STREAM); C(SOCK_DGRAM); C(SOCK_RAW); C(SOCK_RDM); C(SOCK_SEQPACKET);
	C(SOCK_TYPE_MASK); C(SOCK_NONBLOCK); C(SOCK_CLOEXEC);
	C(SOL_SOCKET); C(SOL_IP); C(SOL_TCP); C(SOL_UDP); C(SOL_IPV6);
	C(IPPROTO_IP); C(IPPROTO_ICMP); C(IPPROTO_TCP); C(IPPROTO_UDP); C(IPPROTO_IPV6);
	C(IPPROTO_ICMPV6); C(IPPROTO_RAW);
	C(SO_DEBUG); C(SO_REUSEADDR); C(SO_TYPE); C(SO_ERROR); C(SO_DONTROUTE);
	C(SO_BROADCAST); C(SO_SNDBUF); C(SO_RCVBUF); C(SO_KEEPALIVE); C(SO_OOBINLINE);
	C(SO_NO_CHECK); C(SO_PRIORITY); C(SO_LINGER); C(SO_BSDCOMPAT); C(SO_REUSEPORT);
	C(SO_PASSCRED); C(SO_PEERCRED); C(SO_RCVLOWAT); C(SO_SNDLOWAT);
	C(SO_RCVTIMEO_OLD); C(SO_SNDTIMEO_OLD); C(SO_BINDTODEVICE); C(SO_TIMESTAMP_OLD);
	C(SO_ACCEPTCONN); C(SO_SNDBUFFORCE); C(SO_RCVBUFFORCE); C(SO_PROTOCOL);
	C(SO_DOMAIN); C(SO_RCVTIMEO_NEW); C(SO_SNDTIMEO_NEW);
	C(TCP_NODELAY); C(TCP_MAXSEG); C(TCP_CORK); C(TCP_KEEPIDLE); C(TCP_KEEPINTVL);
	C(TCP_KEEPCNT); C(TCP_INFO); C(TCP_QUICKACK); C(TCP_CONGESTION);
	C(TCP_USER_TIMEOUT); C(TCP_FASTOPEN);
	C(IP_TOS); C(IP_TTL); C(IP_HDRINCL); C(IP_RECVERR); C(IP_MULTICAST_IF);
	C(IP_MULTICAST_TTL); C(IP_MULTICAST_LOOP); C(IP_ADD_MEMBERSHIP);
	C(IP_DROP_MEMBERSHIP);
	C(IPV6_UNICAST_HOPS); C(IPV6_MULTICAST_IF); C(IPV6_MULTICAST_HOPS);
	C(IPV6_MULTICAST_LOOP); C(IPV6_ADD_MEMBERSHIP); C(IPV6_DROP_MEMBERSHIP);
	C(IPV6_V6ONLY); C(IPV6_TCLASS);
	C(MSG_OOB); C(MSG_PEEK); C(MSG_DONTROUTE); C(MSG_CTRUNC); C(MSG_PROBE);
	C(MSG_TRUNC); C(MSG_DONTWAIT); C(MSG_EOR); C(MSG_WAITALL); C(MSG_FIN);
	C(MSG_SYN); C(MSG_CONFIRM); C(MSG_RST); C(MSG_ERRQUEUE); C(MSG_NOSIGNAL);
	C(MSG_MORE); C(MSG_WAITFORONE); C(MSG_FASTOPEN); C(MSG_CMSG_CLOEXEC);
	C(SHUT_RD); C(SHUT_WR); C(SHUT_RDWR);
	C(SCM_RIGHTS); C(SCM_CREDENTIALS); C(SOMAXCONN);
	C(UNIX_PATH_MAX);
	C(VMADDR_CID_ANY); C(VMADDR_CID_HYPERVISOR); C(VMADDR_CID_LOCAL);
	C(VMADDR_CID_HOST); C(VMADDR_PORT_ANY);
	S("sockaddr_in", struct sockaddr_in);
	O("sockaddr_in", struct sockaddr_in, sin_family);
	O("sockaddr_in", struct sockaddr_in, sin_port);
	O("sockaddr_in", struct sockaddr_in, sin_addr);
	O("sockaddr_in", struct sockaddr_in, __pad);
	S("sockaddr_in6", struct sockaddr_in6);
	O("sockaddr_in6", struct sockaddr_in6, sin6_family);
	O("sockaddr_in6", struct sockaddr_in6, sin6_port);
	O("sockaddr_in6", struct sockaddr_in6, sin6_flowinfo);
	O("sockaddr_in6", struct sockaddr_in6, sin6_addr);
	O("sockaddr_in6", struct sockaddr_in6, sin6_scope_id);
	S("sockaddr_un", struct sockaddr_un);
	O("sockaddr_un", struct sockaddr_un, sun_family);
	O("sockaddr_un", struct sockaddr_un, sun_path);
	S("sockaddr_vm", struct sockaddr_vm);
	O("sockaddr_vm", struct sockaddr_vm, svm_family);
	O("sockaddr_vm", struct sockaddr_vm, svm_reserved1);
	O("sockaddr_vm", struct sockaddr_vm, svm_port);
	O("sockaddr_vm", struct sockaddr_vm, svm_cid);
	O("sockaddr_vm", struct sockaddr_vm, svm_flags);
	O("sockaddr_vm", struct sockaddr_vm, svm_zero);
	V("sockaddr_storage.__size", sizeof(struct __kernel_sockaddr_storage));
	S("msghdr", struct user_msghdr);
	O("msghdr", struct user_msghdr, msg_name);
	O("msghdr", struct user_msghdr, msg_namelen);
	O("msghdr", struct user_msghdr, msg_iov);
	O("msghdr", struct user_msghdr, msg_iovlen);
	O("msghdr", struct user_msghdr, msg_control);
	O("msghdr", struct user_msghdr, msg_controllen);
	O("msghdr", struct user_msghdr, msg_flags);
	S("cmsghdr", struct cmsghdr);
	O("cmsghdr", struct cmsghdr, cmsg_len);
	O("cmsghdr", struct cmsghdr, cmsg_level);
	O("cmsghdr", struct cmsghdr, cmsg_type);
	V("CMSG_LEN(4)", CMSG_LEN(4));
	V("CMSG_SPACE(4)", CMSG_SPACE(4));
	V("CMSG_SPACE(12)", CMSG_SPACE(12));
	S("linger", struct linger);
	O("linger", struct linger, l_onoff);
	O("linger", struct linger, l_linger);
	S("ucred", struct ucred);
	O("ucred", struct ucred, pid);
	O("ucred", struct ucred, uid);
	O("ucred", struct ucred, gid);
	S("ip_mreq", struct ip_mreq);
	O("ip_mreq", struct ip_mreq, imr_multiaddr);
	O("ip_mreq", struct ip_mreq, imr_interface);
	S("ipv6_mreq", struct ipv6_mreq);
	O("ipv6_mreq", struct ipv6_mreq, ipv6mr_multiaddr);
	O("ipv6_mreq", struct ipv6_mreq, ipv6mr_ifindex);

	/* ---- misc ---- */
	S("rlimit", struct rlimit);
	O("rlimit", struct rlimit, rlim_cur);
	O("rlimit", struct rlimit, rlim_max);
	S("rlimit64", struct rlimit64);
	C(RLIM_INFINITY); C(RLIM64_INFINITY);
	C(RLIMIT_CPU); C(RLIMIT_FSIZE); C(RLIMIT_DATA); C(RLIMIT_STACK); C(RLIMIT_CORE);
	C(RLIMIT_RSS); C(RLIMIT_NPROC); C(RLIMIT_NOFILE); C(RLIMIT_MEMLOCK); C(RLIMIT_AS);
	C(RLIMIT_LOCKS); C(RLIMIT_SIGPENDING); C(RLIMIT_MSGQUEUE); C(RLIMIT_NICE);
	C(RLIMIT_RTPRIO); C(RLIMIT_RTTIME); C(RLIM_NLIMITS);
	C(RUSAGE_SELF); C(RUSAGE_CHILDREN); C(RUSAGE_THREAD);
	S("rusage", struct rusage);
	O("rusage", struct rusage, ru_utime);
	O("rusage", struct rusage, ru_stime);
	O("rusage", struct rusage, ru_maxrss);
	O("rusage", struct rusage, ru_ixrss);
	O("rusage", struct rusage, ru_idrss);
	O("rusage", struct rusage, ru_isrss);
	O("rusage", struct rusage, ru_minflt);
	O("rusage", struct rusage, ru_majflt);
	O("rusage", struct rusage, ru_nswap);
	O("rusage", struct rusage, ru_inblock);
	O("rusage", struct rusage, ru_oublock);
	O("rusage", struct rusage, ru_msgsnd);
	O("rusage", struct rusage, ru_msgrcv);
	O("rusage", struct rusage, ru_nsignals);
	O("rusage", struct rusage, ru_nvcsw);
	O("rusage", struct rusage, ru_nivcsw);
	S("new_utsname", struct new_utsname);
	O("new_utsname", struct new_utsname, sysname);
	O("new_utsname", struct new_utsname, nodename);
	O("new_utsname", struct new_utsname, release);
	O("new_utsname", struct new_utsname, version);
	O("new_utsname", struct new_utsname, machine);
	O("new_utsname", struct new_utsname, domainname);
	C(__NEW_UTS_LEN);
	S("tms", struct tms);
	O("tms", struct tms, tms_utime);
	O("tms", struct tms, tms_stime);
	O("tms", struct tms, tms_cutime);
	O("tms", struct tms, tms_cstime);
	S("winsize", struct winsize);
	O("winsize", struct winsize, ws_row);
	O("winsize", struct winsize, ws_col);
	O("winsize", struct winsize, ws_xpixel);
	O("winsize", struct winsize, ws_ypixel);
	C(TCGETS); C(TCSETS); C(TCSETSW); C(TCSETSF); C(TIOCSCTTY); C(TIOCGPGRP);
	C(TIOCSPGRP); C(TIOCGWINSZ); C(TIOCSWINSZ); C(FIONREAD); C(TIOCINQ); C(FIONBIO);
	C(TIOCGPTN); C(FIONCLEX); C(FIOCLEX); C(FIOASYNC);
	C(PR_SET_PDEATHSIG); C(PR_GET_PDEATHSIG); C(PR_GET_DUMPABLE); C(PR_SET_DUMPABLE);
	C(PR_SET_NAME); C(PR_GET_NAME); C(PR_GET_TIMERSLACK); C(PR_SET_TIMERSLACK);
	C(PR_SET_NO_NEW_PRIVS); C(PR_GET_NO_NEW_PRIVS); C(PR_SET_THP_DISABLE);
	C(PR_GET_THP_DISABLE); C(PR_SET_VMA); C(PR_SET_VMA_ANON_NAME);
	C(TASK_COMM_LEN);
	C(MEMBARRIER_CMD_QUERY); C(MEMBARRIER_CMD_PRIVATE_EXPEDITED);
	C(MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED);

	/* ---- auxv ---- */
	C(AT_NULL); C(AT_IGNORE); C(AT_EXECFD); C(AT_PHDR); C(AT_PHENT); C(AT_PHNUM);
	C(AT_PAGESZ); C(AT_BASE); C(AT_FLAGS); C(AT_ENTRY); C(AT_NOTELF); C(AT_UID);
	C(AT_EUID); C(AT_GID); C(AT_EGID); C(AT_PLATFORM); C(AT_HWCAP); C(AT_CLKTCK);
	C(AT_SECURE); C(AT_BASE_PLATFORM); C(AT_RANDOM); C(AT_HWCAP2);
	C(AT_RSEQ_FEATURE_SIZE); C(AT_RSEQ_ALIGN); C(AT_HWCAP3); C(AT_HWCAP4);
	C(AT_EXECFN); C(AT_SYSINFO_EHDR); C(AT_MINSIGSTKSZ);
}

#if defined(__x86_64__)
void arch(void)
{
	C(MAP_32BIT); C(MAP_ABOVE4G);
	C(ARCH_SET_GS); C(ARCH_SET_FS); C(ARCH_GET_FS); C(ARCH_GET_GS);
	C(ARCH_GET_CPUID); C(ARCH_SET_CPUID);

	S("stat", struct stat);
	O("stat", struct stat, st_dev);
	O("stat", struct stat, st_ino);
	O("stat", struct stat, st_nlink);
	O("stat", struct stat, st_mode);
	O("stat", struct stat, st_uid);
	O("stat", struct stat, st_gid);
	O("stat", struct stat, __pad0);
	O("stat", struct stat, st_rdev);
	O("stat", struct stat, st_size);
	O("stat", struct stat, st_blksize);
	O("stat", struct stat, st_blocks);
	O("stat", struct stat, st_atime);
	O("stat", struct stat, st_atime_nsec);
	O("stat", struct stat, st_mtime);
	O("stat", struct stat, st_mtime_nsec);
	O("stat", struct stat, st_ctime);
	O("stat", struct stat, st_ctime_nsec);
	O("stat", struct stat, __unused);

	C(UC_FP_XSTATE); C(UC_SIGCONTEXT_SS); C(UC_STRICT_RESTORE_SS);
	C(FP_XSTATE_MAGIC1); C(FP_XSTATE_MAGIC2); C(FP_XSTATE_MAGIC2_SIZE);
	S("sigcontext", struct sigcontext);
	O("sigcontext", struct sigcontext, r8);
	O("sigcontext", struct sigcontext, r9);
	O("sigcontext", struct sigcontext, r10);
	O("sigcontext", struct sigcontext, r11);
	O("sigcontext", struct sigcontext, r12);
	O("sigcontext", struct sigcontext, r13);
	O("sigcontext", struct sigcontext, r14);
	O("sigcontext", struct sigcontext, r15);
	O("sigcontext", struct sigcontext, rdi);
	O("sigcontext", struct sigcontext, rsi);
	O("sigcontext", struct sigcontext, rbp);
	O("sigcontext", struct sigcontext, rbx);
	O("sigcontext", struct sigcontext, rdx);
	O("sigcontext", struct sigcontext, rax);
	O("sigcontext", struct sigcontext, rcx);
	O("sigcontext", struct sigcontext, rsp);
	O("sigcontext", struct sigcontext, rip);
	O("sigcontext", struct sigcontext, eflags);
	O("sigcontext", struct sigcontext, cs);
	O("sigcontext", struct sigcontext, gs);
	O("sigcontext", struct sigcontext, fs);
	O("sigcontext", struct sigcontext, ss);
	O("sigcontext", struct sigcontext, err);
	O("sigcontext", struct sigcontext, trapno);
	O("sigcontext", struct sigcontext, oldmask);
	O("sigcontext", struct sigcontext, cr2);
	O("sigcontext", struct sigcontext, fpstate);
	O("sigcontext", struct sigcontext, reserved1);
	S("_fpx_sw_bytes", struct _fpx_sw_bytes);
	O("_fpx_sw_bytes", struct _fpx_sw_bytes, magic1);
	O("_fpx_sw_bytes", struct _fpx_sw_bytes, extended_size);
	O("_fpx_sw_bytes", struct _fpx_sw_bytes, xfeatures);
	O("_fpx_sw_bytes", struct _fpx_sw_bytes, xstate_size);
	O("_fpx_sw_bytes", struct _fpx_sw_bytes, padding);
	S("_fpstate_64", struct _fpstate_64);
	O("_fpstate_64", struct _fpstate_64, cwd);
	O("_fpstate_64", struct _fpstate_64, swd);
	O("_fpstate_64", struct _fpstate_64, twd);
	O("_fpstate_64", struct _fpstate_64, fop);
	O("_fpstate_64", struct _fpstate_64, rip);
	O("_fpstate_64", struct _fpstate_64, rdp);
	O("_fpstate_64", struct _fpstate_64, mxcsr);
	O("_fpstate_64", struct _fpstate_64, mxcsr_mask);
	O("_fpstate_64", struct _fpstate_64, st_space);
	O("_fpstate_64", struct _fpstate_64, xmm_space);
	O("_fpstate_64", struct _fpstate_64, reserved2);
	O("_fpstate_64", struct _fpstate_64, sw_reserved);
	S("_header", struct _header);
	S("ucontext", struct ucontext);
	O("ucontext", struct ucontext, uc_flags);
	O("ucontext", struct ucontext, uc_link);
	O("ucontext", struct ucontext, uc_stack);
	O("ucontext", struct ucontext, uc_mcontext);
	O("ucontext", struct ucontext, uc_sigmask);
	O("ucontext", struct ucontext, uc_mcontext.rip);
	O("ucontext", struct ucontext, uc_mcontext.rsp);
	O("ucontext", struct ucontext, uc_mcontext.fpstate);
	S("rt_sigframe", struct rt_sigframe);
	O("rt_sigframe", struct rt_sigframe, pretcode);
	O("rt_sigframe", struct rt_sigframe, uc);
	O("rt_sigframe", struct rt_sigframe, info);
	C(FRAME_ALIGNMENT); C(REDZONE); C(FPSTATE_ALIGN);
}
#else
void arch(void)
{
	C(PROT_BTI); C(PROT_MTE);

	S("stat", struct stat);
	O("stat", struct stat, st_dev);
	O("stat", struct stat, st_ino);
	O("stat", struct stat, st_mode);
	O("stat", struct stat, st_nlink);
	O("stat", struct stat, st_uid);
	O("stat", struct stat, st_gid);
	O("stat", struct stat, st_rdev);
	O("stat", struct stat, __pad1);
	O("stat", struct stat, st_size);
	O("stat", struct stat, st_blksize);
	O("stat", struct stat, __pad2);
	O("stat", struct stat, st_blocks);
	O("stat", struct stat, st_atime);
	O("stat", struct stat, st_atime_nsec);
	O("stat", struct stat, st_mtime);
	O("stat", struct stat, st_mtime_nsec);
	O("stat", struct stat, st_ctime);
	O("stat", struct stat, st_ctime_nsec);
	O("stat", struct stat, __unused4);
	O("stat", struct stat, __unused5);

	C(FPSIMD_MAGIC); C(ESR_MAGIC); C(EXTRA_MAGIC); C(SVE_MAGIC); C(TPIDR2_MAGIC);
	C(ZA_MAGIC); C(ZT_MAGIC); C(FPMR_MAGIC); C(POE_MAGIC); C(GCS_MAGIC);
	S("sigcontext", struct sigcontext);
	O("sigcontext", struct sigcontext, fault_address);
	O("sigcontext", struct sigcontext, regs);
	O("sigcontext", struct sigcontext, sp);
	O("sigcontext", struct sigcontext, pc);
	O("sigcontext", struct sigcontext, pstate);
	O("sigcontext", struct sigcontext, __reserved);
	Z("sigcontext", struct sigcontext, __reserved);
	S("_aarch64_ctx", struct _aarch64_ctx);
	O("_aarch64_ctx", struct _aarch64_ctx, magic);
	O("_aarch64_ctx", struct _aarch64_ctx, size);
	S("fpsimd_context", struct fpsimd_context);
	O("fpsimd_context", struct fpsimd_context, head);
	O("fpsimd_context", struct fpsimd_context, fpsr);
	O("fpsimd_context", struct fpsimd_context, fpcr);
	O("fpsimd_context", struct fpsimd_context, vregs);
	S("esr_context", struct esr_context);
	O("esr_context", struct esr_context, head);
	O("esr_context", struct esr_context, esr);
	S("extra_context", struct extra_context);
	S("ucontext", struct ucontext);
	O("ucontext", struct ucontext, uc_flags);
	O("ucontext", struct ucontext, uc_link);
	O("ucontext", struct ucontext, uc_stack);
	O("ucontext", struct ucontext, uc_sigmask);
	O("ucontext", struct ucontext, __unused);
	Z("ucontext", struct ucontext, __unused);
	O("ucontext", struct ucontext, uc_mcontext);
	O("ucontext", struct ucontext, uc_mcontext.regs);
	O("ucontext", struct ucontext, uc_mcontext.sp);
	O("ucontext", struct ucontext, uc_mcontext.pc);
	O("ucontext", struct ucontext, uc_mcontext.__reserved);
	S("rt_sigframe", struct rt_sigframe);
	O("rt_sigframe", struct rt_sigframe, info);
	O("rt_sigframe", struct rt_sigframe, uc);
	O("rt_sigframe", struct rt_sigframe, uc.uc_mcontext.__reserved);
	S("frame_record", struct frame_record);
	O("frame_record", struct frame_record, fp);
	O("frame_record", struct frame_record, lr);
	C(TERMINATOR_SIZE); C(EXTRA_CONTEXT_SIZE);
	/* Records for a plain FPSIMD task (setup_sigframe_layout). */
	V("sigframe.fpsimd_offset", SIGFRAME_FPSIMD_OFFSET);
	V("sigframe.esr_offset", SIGFRAME_ESR_OFFSET);
	V("sigframe.end_offset", SIGFRAME_END_OFFSET);
	V("sigframe.end_offset_noesr", SIGFRAME_END_OFFSET_NOESR);
	V("sigframe.size", SIGFRAME_SIZE);

	C(HWCAP_FP); C(HWCAP_ASIMD); C(HWCAP_EVTSTRM); C(HWCAP_AES); C(HWCAP_PMULL);
	C(HWCAP_SHA1); C(HWCAP_SHA2); C(HWCAP_CRC32); C(HWCAP_ATOMICS); C(HWCAP_FPHP);
	C(HWCAP_ASIMDHP); C(HWCAP_CPUID); C(HWCAP_ASIMDRDM); C(HWCAP_JSCVT);
	C(HWCAP_FCMA); C(HWCAP_LRCPC); C(HWCAP_DCPOP); C(HWCAP_SHA3); C(HWCAP_SM3);
	C(HWCAP_SM4); C(HWCAP_ASIMDDP); C(HWCAP_SHA512); C(HWCAP_SVE); C(HWCAP_ASIMDFHM);
	C(HWCAP_DIT); C(HWCAP_USCAT); C(HWCAP_ILRCPC); C(HWCAP_FLAGM); C(HWCAP_SSBS);
	C(HWCAP_SB); C(HWCAP_PACA); C(HWCAP_PACG);
}
#endif
