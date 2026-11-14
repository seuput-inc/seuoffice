/*
 * Isolated NSS PKCS#12 export helper: one request, one response, then exit.
 *
 * Build/ABI contract (no system NSS lookup): use NSS_3_108_RTM and NSPR
 * 4.36 from reviewed, pinned sources and this NSS build's public headers.
 * Integrate into the official NSS cmd build with USE_STATIC_LIBS=1, using
 * its Linux cmd/platlibs.mk archive ordering. In particular, the nickname
 * lookup is resolved in the matching NSS archive, never from a user's system
 * shared library. The official mixed build still links nssutil3, NSPR and
 * SQLite dynamically: package and verify the exact artifacts used to build,
 * as well as matching NSS softoken/freebl runtime modules. Preserve explicitly
 * configured external PKCS#11 token modules instead of replacing their slot.
 * The launcher must control library paths, strip loader-injection environment,
 * discard diagnostics, verify helper/runtime identities before execution,
 * own the private pipes and kill/reap the entire child tree on cancellation.
 * NSS/NSPR packaging must meet MPL-2.0 notice and source obligations.
 *
 * NSS reference (MPL-2.0):
 * https://github.com/nss-dev/nss/blob/NSS_3_108_RTM/cmd/pk12util/pk12util.c
 * https://github.com/nss-dev/nss/blob/NSS_3_108_RTM/lib/pkcs12/p12.h
 * https://github.com/nss-dev/nss/blob/NSS_3_108_RTM/lib/certdb/certt.h
 * https://github.com/nss-dev/nss/blob/NSS_3_108_RTM/cmd/platlibs.mk
 * https://firefox-source-docs.mozilla.org/security/nss/releases/nss_3_108.html
 * The certificate-list and exact cert->slot logic below follows pk12util; it
 * does not substitute a different token when certificates share a nickname.
 *
 * Request (stdin): version 2, operation 1, three unsigned 64-bit big-endian
 * lengths followed by directory UTF-8, nickname UTF-8, store password UTF-8.
 * Empty store password is allowed. NUL is invalid. No secrets in argv, env,
 * files or diagnostics. Responses use one status byte and two u64 BE
 * lengths. Status 5 requests protected-token confirmation: length 1 is an
 * untrusted token-name byte string and length 2 is zero; the parent replies
 * [2,1] to confirm or [2,0] to deny. EOF, malformed replies and cancellation
 * deny. The helper may request confirmation more than once; no PIN crosses
 * this protocol. Final status 0 success carries PKCS#12 and password bytes;
 * 1 is locked, 2 not exportable, 3 unavailable, and 4 malformed request.
 * The parent binds each prompt to the same native authority, rechecks after
 * the dialog, treats token names as plain text, and bounds frame allocations.
 * Pipes are inherited, dedicated, anonymous and never renderer-controlled.
 */
#define _POSIX_C_SOURCE 200809L
#include <errno.h>
#include <limits.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <cert.h>
#include <certdb.h>
#include <nss.h>
#include <pk11pub.h>
#include <p12.h>
#include <secerr.h>
#include <secitem.h>
#include <secoidt.h>
#include <secport.h>

#define VERSION 2
#define EXPORT_OP 1
#define OK 0
#define LOCKED 1
#define NOT_EXPORTABLE 2
#define UNAVAILABLE 3
#define BAD_REQUEST 4
#define CONFIRM_PROTECTED 5

struct bytes { unsigned char *data; size_t len; size_t capacity; int failed; };
struct password { char *data; };

static void wipe(void *ptr, size_t len) {
    volatile unsigned char *p = (volatile unsigned char *)ptr;
    while (len--) *p++ = 0;
}
static int read_full(int fd, void *buf, size_t len) {
    unsigned char *p = buf;
    while (len) {
        ssize_t n = read(fd, p, len > (size_t)SSIZE_MAX ? (size_t)SSIZE_MAX : len);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return 0;
        p += (size_t)n; len -= (size_t)n;
    }
    return 1;
}
static int write_full(int fd, const void *buf, size_t len) {
    const unsigned char *p = buf;
    while (len) {
        ssize_t n = write(fd, p, len > (size_t)SSIZE_MAX ? (size_t)SSIZE_MAX : len);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return 0;
        p += (size_t)n; len -= (size_t)n;
    }
    return 1;
}
static uint64_t be64(const unsigned char *p) {
    uint64_t n = 0; int i;
    for (i = 0; i < 8; ++i) n = (n << 8) | p[i];
    return n;
}
static void put64(unsigned char *p, uint64_t n) {
    int i; for (i = 7; i >= 0; --i) { p[i] = (unsigned char)n; n >>= 8; }
}
static void reply(unsigned char status, const struct bytes *p12, const char *password) {
    unsigned char header[17] = {0};
    size_t pass_len = status == OK ? strlen(password) : 0;
    size_t p12_len = status == OK ? p12->len : 0;
    header[0] = status; put64(header + 1, (uint64_t)p12_len); put64(header + 9, (uint64_t)pass_len);
    if (!write_full(STDOUT_FILENO, header, sizeof header)) return;
    if (p12_len && !write_full(STDOUT_FILENO, p12->data, p12_len)) return;
    if (pass_len) (void)write_full(STDOUT_FILENO, password, pass_len);
}
static int confirm_protected_token(const char *token) {
    unsigned char header[17] = {0}, answer[2];
    size_t len = token ? strnlen(token, 4097) : 0;
    if (len > 4096) return 0;
    header[0] = CONFIRM_PROTECTED;
    put64(header + 1, (uint64_t)len);
    if (!write_full(STDOUT_FILENO, header, sizeof header) ||
        (len && !write_full(STDOUT_FILENO, token, len)) ||
        !read_full(STDIN_FILENO, answer, sizeof answer)) return 0;
    return answer[0] == VERSION && answer[1] == 1;
}
static char *get_field(size_t len) {
    char *out;
    if (len == SIZE_MAX) return NULL;
    out = malloc(len + 1);
    if (!out) return NULL;
    if (!read_full(STDIN_FILENO, out, len) || memchr(out, 0, len)) {
        wipe(out, len + 1); free(out); return NULL;
    }
    out[len] = 0; return out;
}
static char *password_callback(PK11SlotInfo *slot, PRBool retry, void *arg) {
    struct password *pw = arg;
    const char *token, *line, *end, *chosen = NULL;
    size_t token_len, chosen_len = 0, total;
    char *copy;
    if (retry) return NULL;
    if (PK11_ProtectedAuthenticationPath(slot)) {
        return confirm_protected_token(PK11_GetTokenName(slot)) ? PORT_Strdup("external") : NULL;
    }
    if (!pw || !pw->data || !*pw->data) return NULL;
    token = PK11_GetTokenName(slot);
    token_len = token ? strlen(token) : 0;
    total = strlen(pw->data);
    if (total > 4095) total = 4095; /* pk12util reads at most 4096 bytes including its newline. */
    line = pw->data;
    while ((size_t)(line - pw->data) < total) {
        end = line;
        while ((size_t)(end - pw->data) < total && *end != '\r' && *end != '\n') ++end;
        chosen = line; chosen_len = (size_t)(end - line);
        if (!token || (chosen_len > token_len && !memcmp(line, token, token_len) && line[token_len] == ':')) {
            if (token) { chosen += token_len + 1; chosen_len -= token_len + 1; }
            break;
        }
        while ((size_t)(end - pw->data) < total && (*end == '\r' || *end == '\n')) ++end;
        line = end;
    }
    if (!chosen) return NULL;
    copy = PORT_Alloc(chosen_len + 1);
    if (copy) { memcpy(copy, chosen, chosen_len); copy[chosen_len] = 0; }
    return copy;
}
static PRBool unicode_callback(PRBool to_unicode, unsigned char *in, unsigned int in_len,
                               unsigned char *out, unsigned int max_len,
                               unsigned int *out_len, PRBool swap) {
    SECItem item = { siBuffer, in, in_len };
    SECItem *copy = SECITEM_DupItem(&item);
    PRBool result;
    unsigned int i;
    if (!copy) return PR_FALSE;
    if (!to_unicode && swap) {
        if (copy->len % 2) { SECITEM_ZfreeItem(copy, PR_TRUE); return PR_FALSE; }
        for (i = 0; i < copy->len; i += 2) {
            unsigned char byte = copy->data[i]; copy->data[i] = copy->data[i + 1]; copy->data[i + 1] = byte;
        }
    }
    result = PORT_UCS2_UTF8Conversion(to_unicode, copy->data, copy->len, out, max_len, out_len);
    SECITEM_ZfreeItem(copy, PR_TRUE); return result;
}
static void output_callback(void *arg, const char *buf, unsigned long length) {
    struct bytes *out = arg;
    size_t len = (size_t)length, capacity;
    unsigned char *next;
    if (out->failed || (unsigned long)len != length || len > SIZE_MAX - out->len) { out->failed = 1; return; }
    if (out->len + len > out->capacity) {
        capacity = out->capacity && out->capacity <= SIZE_MAX / 2 ? out->capacity * 2 : out->len + len;
        if (capacity < out->len + len) capacity = out->len + len;
        next = malloc(capacity);
        if (!next) { out->failed = 1; return; }
        if (out->len) memcpy(next, out->data, out->len);
        if (out->data) { wipe(out->data, out->capacity); free(out->data); }
        out->data = next; out->capacity = capacity;
    }
    if (len) memcpy(out->data + out->len, buf, len);
    out->len += len;
}
static unsigned char classify_error(void) {
    switch (PORT_GetError()) {
        case SEC_ERROR_BAD_PASSWORD: case SEC_ERROR_RETRY_PASSWORD:
        case SEC_ERROR_USER_CANCELLED: case SEC_ERROR_INVALID_PASSWORD:
        case SEC_ERROR_TOKEN_NOT_LOGGED_IN: case SEC_ERROR_EXPIRED_PASSWORD:
        case SEC_ERROR_LOCKED_PASSWORD: return LOCKED;
        default: return NOT_EXPORTABLE;
    }
}
static unsigned char export_identity(const char *dir, const char *nick, struct password *store,
                                     struct bytes *output, char generated[65]) {
    unsigned char status = NOT_EXPORTABLE, random[32] = {0};
    char *db = NULL;
    size_t dir_len = strlen(dir), i;
    static const char hex[] = "0123456789abcdef";
    SECItem pass;
    PK11SlotInfo *internal = NULL, *slot;
    CERTCertList *certs = NULL;
    CERTCertListNode *node;
    SEC_PKCS12ExportContext *context = NULL;
    SEC_PKCS12SafeInfo *key_safe, *cert_safe;
    int initialized = 0;
    if (dir_len > SIZE_MAX - 5) goto done;
    db = malloc(dir_len + 5); if (!db) goto done;
    memcpy(db, "sql:", 4); memcpy(db + 4, dir, dir_len + 1);
    PK11_SetPasswordFunc(password_callback);
    if (NSS_Initialize(db, "", "", "secmod.db", 0) != SECSuccess) { status = classify_error(); goto done; }
    initialized = 1;
    PORT_SetUCS2_ASCIIConversionFunction(unicode_callback);
    internal = PK11_GetInternalKeySlot(); if (!internal) { status = classify_error(); goto done; }
    if (PK11_Authenticate(internal, PR_TRUE, store) != SECSuccess) { status = LOCKED; goto done; }
    certs = PK11_FindCertsFromNickname(nick, store);
    if (!certs) { status = classify_error(); goto done; }
    if (CERT_FilterCertListForUserCerts(certs) != SECSuccess || CERT_LIST_EMPTY(certs)) { status = classify_error(); goto done; }
    if (PK11_GenerateRandom(random, sizeof random) != SECSuccess) { status = classify_error(); goto done; }
    for (i = 0; i < sizeof random; ++i) { generated[2*i] = hex[random[i] >> 4]; generated[2*i+1] = hex[random[i] & 15]; }
    generated[64] = 0;
    pass.type = siBuffer; pass.data = (unsigned char *)generated; pass.len = 64;
    if (!CERT_LIST_HEAD(certs)->cert) goto done;
    slot = CERT_LIST_HEAD(certs)->cert->slot;
    if (!slot) goto done;
    context = SEC_PKCS12CreateExportContext(NULL, NULL, slot, store);
    if (!context) { status = classify_error(); goto done; }
    if (SEC_PKCS12AddPasswordIntegrity(context, &pass, PK11_IsFIPS() ? SEC_OID_HMAC_SHA256 : SEC_OID_SHA256) != SECSuccess) { status = classify_error(); goto done; }
    for (node = CERT_LIST_HEAD(certs); !CERT_LIST_END(node, certs); node = CERT_LIST_NEXT(node)) {
        CERTCertificate *cert = node->cert;
        if (!cert || !cert->slot) goto done;
        key_safe = SEC_PKCS12CreateUnencryptedSafe(context);
        cert_safe = SEC_PKCS12CreatePasswordPrivSafe(context, &pass, SEC_OID_AES_128_CBC);
        if (!key_safe || !cert_safe || SEC_PKCS12AddCertAndKey(context, cert_safe, NULL, cert,
                CERT_GetDefaultCertDB(), key_safe, NULL, PR_TRUE, &pass, SEC_OID_AES_256_CBC) != SECSuccess) {
            status = classify_error(); goto done;
        }
    }
    if (SEC_PKCS12Encode(context, output_callback, output) != SECSuccess || output->failed || !output->len) { status = classify_error(); goto done; }
    status = OK;
done:
    if (context) SEC_PKCS12DestroyExportContext(context);
    if (certs) CERT_DestroyCertList(certs);
    if (internal) PK11_FreeSlot(internal);
    if (initialized) (void)NSS_Shutdown();
    PK11_SetPasswordFunc(NULL);
    wipe(random, sizeof random);
    free(db);
    return status;
}
int main(void) {
    unsigned char header[26], status = BAD_REQUEST;
    uint64_t lengths[3]; size_t i;
    char *dir = NULL, *nick = NULL, *store = NULL;
    char generated[65] = {0};
    struct password password = { NULL };
    struct bytes output = { NULL, 0, 0, 0 };
    if (!read_full(STDIN_FILENO, header, sizeof header) || header[0] != VERSION || header[1] != EXPORT_OP) goto done;
    for (i = 0; i < 3; ++i) {
        lengths[i] = be64(header + 2 + 8*i);
        if (lengths[i] >= SIZE_MAX || lengths[i] > UINT_MAX) goto done;
    }
    dir = get_field((size_t)lengths[0]); if (!dir) goto done;
    nick = get_field((size_t)lengths[1]); if (!nick) goto done;
    store = get_field((size_t)lengths[2]); if (!store) goto done;
    password.data = store;
    status = export_identity(dir, nick, &password, &output, generated);
done:
    reply(status, &output, generated);
    if (store) { wipe(store, strlen(store)); free(store); }
    if (output.data) { wipe(output.data, output.capacity); free(output.data); }
    wipe(generated, sizeof generated);
    free(nick); free(dir);
    return status == OK ? 0 : 1;
}
