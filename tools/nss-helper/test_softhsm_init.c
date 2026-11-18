/* Hosted-only synthetic SoftHSM initialization. The fixed PINs are test data,
 * not user credentials. Module/config paths are passed by the isolated test. */
#include <dlfcn.h>
#include <stdio.h>
#include <string.h>
#include <pkcs11.h>
#include <pkcs11t.h>
static int check(CK_RV rv, const char *step) {
    if (rv == CKR_OK) return 1;
    fprintf(stderr, "%s: 0x%lx\n", step, (unsigned long)rv); return 0;
}
int main(int argc, char **argv) {
    void *library; CK_C_GetFunctionList get_list; CK_FUNCTION_LIST_PTR api;
    CK_SLOT_ID slots[64], selected = 0; CK_ULONG count = 64, i;
    CK_SESSION_HANDLE session; CK_TOKEN_INFO info;
    CK_UTF8CHAR label[32], pin[] = "1234", so_pin[] = "5678";
    if (argc != 2) return 2;
    library = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL); if (!library) return 2;
    *(void **)(&get_list) = dlsym(library, "C_GetFunctionList");
    if (!get_list || !check(get_list(&api), "function list") || !api) return 2;
    if (!check(api->C_Initialize(NULL), "initialize") || !check(api->C_GetSlotList(CK_TRUE, slots, &count), "slots")) return 2;
    for (i = 0; i < count; ++i) {
        if (api->C_GetTokenInfo(slots[i], &info) == CKR_OK && !(info.flags & CKF_TOKEN_INITIALIZED)) { selected = slots[i]; break; }
    }
    if (i == count) return 2;
    memset(label, ' ', sizeof label); memcpy(label, "SyntheticExternal", 17);
    if (!check(api->C_InitToken(selected, so_pin, sizeof so_pin - 1, label), "init token")) return 2;
    /* SoftHSM may assign the initialized token a new slot ID. */
    count = 64;
    if (!check(api->C_GetSlotList(CK_TRUE, slots, &count), "initialized slots")) return 2;
    for (i = 0; i < count; ++i) {
        if (api->C_GetTokenInfo(slots[i], &info) == CKR_OK && memcmp(info.label, label, sizeof label) == 0) { selected = slots[i]; break; }
    }
    if (i == count || !check(api->C_OpenSession(selected, CKF_SERIAL_SESSION | CKF_RW_SESSION, NULL, NULL, &session), "session")) return 2;
    if (!check(api->C_Login(session, CKU_SO, so_pin, sizeof so_pin - 1), "SO login") ||
        !check(api->C_InitPIN(session, pin, sizeof pin - 1), "init PIN") ||
        !check(api->C_Logout(session), "logout") || !check(api->C_CloseSession(session), "close") ||
        !check(api->C_Finalize(NULL), "finalize")) return 2;
    dlclose(library); return 0;
}
