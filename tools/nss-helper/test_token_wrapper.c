/* Hosted-only protected-auth proxy over real pinned SoftHSM2. Crypto, keys,
 * certificates and export are delegated to the real provider. The fixed PIN
 * is synthetic test data; no real token or hardware PIN is consulted. */
#define _POSIX_C_SOURCE 200809L
#include <dlfcn.h>
#include <stdlib.h>
#include <unistd.h>
#include <pkcs11.h>
#include <pkcs11t.h>
#ifndef TEST_PROVIDER_PATH
#error TEST_PROVIDER_PATH required
#endif
#ifndef TEST_PROVIDER_CONFIG
#error TEST_PROVIDER_CONFIG required
#endif
#ifndef TEST_PROTECTED_MARKER
#error TEST_PROTECTED_MARKER required
#endif
static CK_FUNCTION_LIST_PTR upstream;
static CK_FUNCTION_LIST wrapped;
static int synthetic_logged_in;
static int protected_enabled(void) { return access(TEST_PROTECTED_MARKER, F_OK) == 0; }
static CK_RV token_info(CK_SLOT_ID id, CK_TOKEN_INFO_PTR info) {
    CK_RV rv = upstream->C_GetTokenInfo(id, info);
    if (rv == CKR_OK && protected_enabled()) info->flags |= CKF_PROTECTED_AUTHENTICATION_PATH | CKF_LOGIN_REQUIRED;
    return rv;
}
static CK_RV session_info(CK_SESSION_HANDLE session, CK_SESSION_INFO_PTR info) {
    CK_RV rv = upstream->C_GetSessionInfo(session, info);
    if (rv == CKR_OK && protected_enabled() && !synthetic_logged_in) {
        if (info->state == CKS_RO_USER_FUNCTIONS) info->state = CKS_RO_PUBLIC_SESSION;
        if (info->state == CKS_RW_USER_FUNCTIONS) info->state = CKS_RW_PUBLIC_SESSION;
    }
    return rv;
}
static CK_RV login(CK_SESSION_HANDLE session, CK_USER_TYPE user, CK_UTF8CHAR_PTR pin, CK_ULONG length) {
    CK_RV rv;
    if (!protected_enabled()) return upstream->C_Login(session, user, pin, length);
    { static CK_UTF8CHAR synthetic_pin[] = "1234"; rv = upstream->C_Login(session, user, synthetic_pin, sizeof synthetic_pin - 1); }
    if (rv == CKR_OK || rv == CKR_USER_ALREADY_LOGGED_IN) synthetic_logged_in = 1;
    return rv;
}
CK_RV C_GetFunctionList(CK_FUNCTION_LIST_PTR_PTR output) {
    void *handle;
    CK_C_GetFunctionList get_list;
    CK_RV rv;
    if (!output) return CKR_ARGUMENTS_BAD;
    if (setenv("SOFTHSM2_CONF", TEST_PROVIDER_CONFIG, 1) != 0) return CKR_GENERAL_ERROR;
    handle = dlopen(TEST_PROVIDER_PATH, RTLD_NOW | RTLD_LOCAL);
    if (!handle) return CKR_GENERAL_ERROR;
    *(void **)(&get_list) = dlsym(handle, "C_GetFunctionList");
    if (!get_list) return CKR_GENERAL_ERROR;
    rv = get_list(&upstream);
    if (rv != CKR_OK || !upstream) return rv == CKR_OK ? CKR_GENERAL_ERROR : rv;
    wrapped = *upstream;
    wrapped.C_GetTokenInfo = token_info;
    wrapped.C_GetSessionInfo = session_info;
    wrapped.C_Login = login;
    *output = &wrapped;
    return CKR_OK;
}
