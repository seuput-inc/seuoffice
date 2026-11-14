#!/usr/bin/env python3
"""Patch only NSS_3_108_RTM private internal module resolution.

Inputs have to be verified against the official release SHA256 by the caller.
The launcher supplies sealed inherited descriptors from contract.json.
Configured external PKCS#11 modules still use NSS's original loader.
"""
import json
import re
import pathlib
import sys

root = pathlib.Path(sys.argv[1]).resolve(strict=True)
contract = json.loads((pathlib.Path(__file__).resolve().parent / 'contract.json').read_text())
fds = {item['id']: item['fd'] for item in contract['artifacts']}
if contract['format'] != 1 or len(fds) != len(contract['artifacts']) or len(set(fds.values())) != len(fds):
    raise SystemExit('invalid NSS descriptor contract')

def replace(relative, old, new):
    path = root / relative
    source = path.read_text(encoding='utf-8')
    if source.count(old) != 1:
        raise SystemExit(f'unexpected NSS source at {relative}')
    new = re.sub(r'@FD:([^@]+)@', lambda match: str(fds[match.group(1)]), new)
    path.write_text(source.replace(old, new, 1), encoding='utf-8')

def replace_region(relative, start, end, replacement):
    path = root / relative
    source = path.read_text(encoding='utf-8')
    if source.count(start) != 1 or source.count(end) != 1:
        raise SystemExit(f'unexpected NSS region at {relative}')
    begin, finish = source.index(start), source.index(end)
    if begin >= finish:
        raise SystemExit(f'bad NSS region ordering at {relative}')
    replacement = re.sub(r'@FD:([^@]+)@', lambda match: str(fds[match.group(1)]), replacement)
    path.write_text(source[:begin] + replacement + source[finish:], encoding='utf-8')

def replace_tail(relative, start, expected_end, replacement):
    path = root / relative
    source = path.read_text(encoding='utf-8')
    if source.count(start) != 1 or not source.rstrip().endswith(expected_end):
        raise SystemExit(f'unexpected NSS tail at {relative}')
    replacement = re.sub(r'@FD:([^@]+)@', lambda match: str(fds[match.group(1)]), replacement)
    path.write_text(source[:source.index(start)] + replacement, encoding='utf-8')

replace('lib/pk11wrap/pk11load.c',
'''    handle = PORT_LoadLibraryFromOrigin(my_shlib_name,
                                        (PRFuncPtr)&softoken_LoadDSO,
                                        softoken_shlib_name);''',
'''    /* Pinned internal softoken only. The parent supplies a verified sealed FD. */
    handle = PR_LoadLibrary("/proc/self/fd/@FD:libsoftokn3.so@");''')

replace('lib/pk11wrap/pk11load.c',
'''static const char *my_shlib_name =
    SHLIB_PREFIX "nss" NSS_SHLIB_VERSION "." SHLIB_SUFFIX;
static const char *softoken_shlib_name =
    SHLIB_PREFIX "softokn" SOFTOKEN_SHLIB_VERSION "." SHLIB_SUFFIX;
''', '')
replace('lib/freebl/loader.c',
'''static const char *NameOfThisSharedLib =
    SHLIB_PREFIX "softokn" SOFTOKEN_SHLIB_VERSION "." SHLIB_SUFFIX;

''', '')

replace('lib/freebl/lowhash_vector.c',
'''static char *
freebl_lowhash_getLibraryFilePath(void *addr)
{
    Dl_info dli;
    if (dladdr(addr, &dli) == 0) {
        return NULL;
    }
    return strdup(dli.dli_fname);
}

''', '')
replace('lib/freebl/lowhash_vector.c',
'''#define PR_GetLibraryFilePathname(name, addr) \\
    freebl_lowhash_getLibraryFilePath(addr)
''', '')

replace_tail('lib/freebl/genload.c',
'''#ifdef XP_UNIX
#include <unistd.h>''',
'''    return lib;
}''',
'''#ifdef XP_UNIX
#include <unistd.h>
#endif
/* Private module identity is fixed by the inherited descriptor contract. */
static PRLibrary*
loader_LoadLibrary(const char* nameToLoad)
{
    PRLibSpec libSpec;
    libSpec.type = PR_LibSpec_Pathname;
    if (strcmp(nameToLoad, "libfreebl3.so") == 0) {
        libSpec.value.pathname = "/proc/self/fd/@FD:libfreebl3.so@";
    } else if (strcmp(nameToLoad, "libfreeblpriv3.so") == 0) {
        libSpec.value.pathname = "/proc/self/fd/@FD:libfreeblpriv3.so@";
    } else {
        return NULL;
    }
    return PR_LoadLibraryWithFlags(libSpec, PR_LD_NOW | PR_LD_LOCAL);
}
''')

replace('lib/freebl/shvfy.c',
'''    int ln_len = PORT_Strlen(libName);
    int index = ln_len + 1 - sizeof("." SHLIB_SUFFIX);''',
'''    /* The verified private libraries and their final .chk files are sealed
     * separately. Never derive an adjacent pathname from an fd symlink. */
    if (PORT_Strcmp(libName, "/proc/self/fd/@FD:libsoftokn3.so@") == 0) return PORT_Strdup("/proc/self/fd/@FD:libsoftokn3.chk@");
    if (PORT_Strcmp(libName, "/proc/self/fd/@FD:libfreebl3.so@") == 0) return PORT_Strdup("/proc/self/fd/@FD:libfreebl3.chk@");
    if (PORT_Strcmp(libName, "/proc/self/fd/@FD:libfreeblpriv3.so@") == 0) return PORT_Strdup("/proc/self/fd/@FD:libfreeblpriv3.chk@");
    if (PORT_Strcmp(libName, "/proc/self/fd/@FD:libnssdbm3.so@") == 0) return PORT_Strdup("/proc/self/fd/@FD:libnssdbm3.chk@");
    int ln_len = PORT_Strlen(libName);
    int index = ln_len + 1 - sizeof("." SHLIB_SUFFIX);''')
replace('lib/freebl/shvfy.c',
'''    /* find our shared library name */
    char *shName = PR_GetLibraryFilePathname(name, addr);''',
'''    /* Address-based resolution of a memfd is not a stable pathname. */
    char *shName;
    if (name && PORT_Strstr(name, "softokn3")) {
        shName = PORT_Strdup("/proc/self/fd/@FD:libsoftokn3.so@");
    } else if (name && PORT_Strstr(name, "freeblpriv3")) {
        shName = PORT_Strdup("/proc/self/fd/@FD:libfreeblpriv3.so@");
    } else if (name && PORT_Strstr(name, "freebl3")) {
        shName = PORT_Strdup("/proc/self/fd/@FD:libfreebl3.so@");
    } else if (name && PORT_Strstr(name, "nssdbm3")) {
        shName = PORT_Strdup("/proc/self/fd/@FD:libnssdbm3.so@");
    } else {
        shName = PR_GetLibraryFilePathname(name, addr);
    }''')

replace_region('lib/softoken/lgglue.c',
'''/*
 * The following 3 functions duplicate the work done by bl_LoadLibrary.''',
'''/*
 * stub files for legacy db's to be able to encrypt and decrypt
 * various keys and attributes.
 */
static SECStatus
sftkdb_encrypt_stub''',
'''/* The private DBM module comes from the verified inherited descriptor. */
static PRLibrary *
sftkdb_LoadLibrary(const char *libname)
{
    if (PORT_Strcmp(libname, "libnssdbm3.so") != 0) return NULL;
    return PR_LoadLibrary("/proc/self/fd/@FD:libnssdbm3.so@");
}

''')

replace('lib/softoken/fipstokn.c',
'''    libaudit_handle = dlopen(LIBAUDIT_NAME, RTLD_LAZY);''',
'''    /* libaudit is an OS audit-service dependency, not a private crypto
     * module. Restrict it to trusted system installation directories; never
     * consult the current directory or an inherited loader search path. */
#if defined(__x86_64__)
    libaudit_handle = dlopen("/usr/lib/x86_64-linux-gnu/libaudit.so.1", RTLD_LAZY);
#elif defined(__aarch64__)
    libaudit_handle = dlopen("/usr/lib/aarch64-linux-gnu/libaudit.so.1", RTLD_LAZY);
#endif
    if (!libaudit_handle) libaudit_handle = dlopen("/usr/lib64/libaudit.so.1", RTLD_LAZY);
    if (!libaudit_handle) libaudit_handle = dlopen("/usr/lib/libaudit.so.1", RTLD_LAZY);''')
