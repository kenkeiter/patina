/* Minimal libc stand-in: linux/vm_sockets.h includes <sys/socket.h> for
 * sa_family_t and struct sockaddr (glibc defines them there). */
#ifndef PATINA_STUB_SYS_SOCKET_H
#define PATINA_STUB_SYS_SOCKET_H
#include <linux/socket.h>
typedef __kernel_sa_family_t sa_family_t;
struct sockaddr {
	sa_family_t sa_family;
	char sa_data[14];
};
#endif
