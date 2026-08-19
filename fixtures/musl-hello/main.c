/* Integration fixture: a dynamically linked musl program.
 *
 * musl differs from glibc in the ways that matter to a packager: the loader and
 * libc are the same file (`libc.musl-x86_64.so.1` is a symlink to
 * `ld-musl-x86_64.so.1`), there is no `ld.so.cache`, and name resolution lives
 * in libc instead of dlopen'ed NSS modules. */

#include <netdb.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>

int main(void) {
    printf("hello from musl\n");

    struct addrinfo hints;
    struct addrinfo *result = NULL;
    memset(&hints, 0, sizeof(hints));
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;

    int rc = getaddrinfo("example.com", "443", &hints, &result);
    printf("dns:%s\n", rc == 0 ? "ok" : "fail");
    if (result != NULL) {
        freeaddrinfo(result);
    }
    return 0;
}
