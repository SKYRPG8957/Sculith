/* Linux-only syscall fault injection. Never linked into the server. */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dlfcn.h>
#include <errno.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

ssize_t sendto(int fd, const void *buf, size_t len, int flags,
               const struct sockaddr *dest, socklen_t addrlen) {
    const char *target = getenv("SCULITH_TEST_DENY_IP");
    const char *gate = getenv("SCULITH_TEST_DENY_FILE");
    struct in_addr ip;
    int armed = !gate || access(gate, F_OK) == 0;
    if (armed && target && dest && dest->sa_family == AF_INET &&
        (strcmp(target, "*") == 0 ||
         (inet_pton(AF_INET, target, &ip) == 1 &&
          ((const struct sockaddr_in *)dest)->sin_addr.s_addr == ip.s_addr))) {
        static atomic_int reported;
        if (!atomic_exchange(&reported, 1)) {
            const char note[] = "TEST: sendto candidate rejected with EPERM\n";
            if (write(2, note, sizeof(note) - 1) != (ssize_t)(sizeof(note) - 1)) {
                errno = EIO;
                return -1;
            }
        }
        errno = EPERM;
        return -1;
    }
    typedef ssize_t (*sendto_fn)(int, const void *, size_t, int,
                                 const struct sockaddr *, socklen_t);
    sendto_fn real_sendto = (sendto_fn)dlsym(RTLD_NEXT, "sendto");
    return real_sendto(fd, buf, len, flags, dest, addrlen);
}
