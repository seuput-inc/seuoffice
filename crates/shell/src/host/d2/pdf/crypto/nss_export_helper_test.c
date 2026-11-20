/* Hosted-only synthetic protected-PKCS#11 callback regression. Compile with
 * the same pinned NSS headers/libraries and build flags as the helper. This
 * test replaces only the token's protected-path/name queries; it never opens a
 * real token, NSS database, user file, or UI. Not an end-to-end UI proof. */
#define _POSIX_C_SOURCE 200809L
#include <assert.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>
#include <pk11pub.h>

static PRBool synthetic_protected(PK11SlotInfo *slot) { (void)slot; return PR_TRUE; }
static char *synthetic_name(PK11SlotInfo *slot) { (void)slot; return "Synthetic <token>"; }
#define PK11_ProtectedAuthenticationPath synthetic_protected
#define PK11_GetTokenName synthetic_name
#define main nss_export_helper_entry
#include "nss_export_helper.c"
#undef main

static void confirmation_case(int answer, int expected) {
    int request[2], response[2], status;
    pid_t child;
    unsigned char header[17], token[17], reply_bytes[2] = {2, (unsigned char)answer};
    assert(pipe(request) == 0 && pipe(response) == 0);
    child = fork(); assert(child >= 0);
    if (child == 0) {
        char *result;
        close(request[1]); close(response[0]);
        if (dup2(request[0], STDIN_FILENO) < 0 || dup2(response[1], STDOUT_FILENO) < 0) _exit(2);
        close(request[0]); close(response[1]);
        if (password_callback((PK11SlotInfo *)1, PR_TRUE, NULL) != NULL) _exit(3);
        result = password_callback((PK11SlotInfo *)1, PR_FALSE, NULL);
        if (expected) {
            if (!result || strcmp(result, "external") != 0) _exit(4);
        } else if (result) _exit(5);
        if (result) PORT_Free(result);
        _exit(0);
    }
    close(request[0]); close(response[1]);
    assert(read_full(response[0], header, sizeof header));
    assert(header[0] == CONFIRM_PROTECTED && be64(header+1) == sizeof token && be64(header+9) == 0);
    assert(read_full(response[0], token, sizeof token));
    assert(memcmp(token, "Synthetic <token>", sizeof token) == 0);
    if (answer >= 0) assert(write_full(request[1], reply_bytes, sizeof reply_bytes));
    close(request[1]); close(response[0]);
    assert(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
}
int main(void) {
    confirmation_case(1, 1);
    confirmation_case(0, 0);
    confirmation_case(7, 0);
    confirmation_case(-1, 0);
    return 0;
}
