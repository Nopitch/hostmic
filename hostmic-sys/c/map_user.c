/*
 * hostmic-sys: the one routine the driver needs in C.
 *
 * MmMapLockedPagesSpecifyCache raises a structured exception when it cannot map pages into
 * user space ("Callers that specify UserMode must wrap the call ... in a try/except block"),
 * and Rust cannot catch structured exceptions. This wrapper turns the exception into a status.
 */

#include <ntddk.h>

/*
 * Which exceptions the wrapper handles: all but a breakpoint or an assertion failure. Driver
 * Verifier reports some violations by breaking into the debugger, and NT_ASSERT does the same;
 * without a debugger, they are exceptions, and handling them here would turn a reported defect
 * into a quiet failure. Left alone, they stop the system (bug check 0x1E with 0x80000003 or
 * 0xC0000420), as they are meant to.
 */
static int hostmic_map_filter(NTSTATUS code)
{
    return code == STATUS_BREAKPOINT || code == STATUS_ASSERTION_FAILURE
               ? EXCEPTION_CONTINUE_SEARCH
               : EXCEPTION_EXECUTE_HANDLER;
}

/*
 * Maps the pages of `mdl` into the current process, readable and writable, with `cache`.
 * Stores the user address in `*address` and returns STATUS_SUCCESS; on failure stores NULL and
 * returns the exception's code (a failure status), so that the reason reaches the driver's log
 * and the host. IRQL <= APC_LEVEL, in the context of the process that gets the mapping.
 *
 * MdlMappingNoExecute: a user-mode mapping is never executable ("A non-executable mapping is
 * always created when AccessMode is UserMode"), yet Driver Verifier's code integrity checks
 * refuse a request without the flag ("specified an executable mapping"), as HVCI may: the
 * flag states it.
 */
NTSTATUS hostmic_map_user(PMDL mdl, MEMORY_CACHING_TYPE cache, PVOID *address)
{
    PVOID mapped = NULL;
    NTSTATUS status = STATUS_SUCCESS;
    __try {
        mapped = MmMapLockedPagesSpecifyCache(mdl, UserMode, cache, NULL, FALSE,
                                              NormalPagePriority | MdlMappingNoExecute);
    } __except (hostmic_map_filter(GetExceptionCode())) {
        mapped = NULL;
        status = GetExceptionCode();
        if (NT_SUCCESS(status)) {
            status = STATUS_UNSUCCESSFUL;
        }
    }
    if (NT_SUCCESS(status) && mapped == NULL) {
        status = STATUS_INSUFFICIENT_RESOURCES;
    }
    *address = mapped;
    return status;
}
