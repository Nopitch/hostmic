/*
 * hostmic-sys: the one routine the driver needs in C.
 *
 * MmMapLockedPagesSpecifyCache raises a structured exception when it cannot map pages into
 * user space ("Callers that specify UserMode must wrap the call ... in a try/except block"),
 * and Rust cannot catch structured exceptions. This wrapper turns the exception into NULL.
 */

#include <ntddk.h>

/*
 * Maps the pages of `mdl` into the current process, readable and writable, with `cache`.
 * User-mode mappings are never executable. Returns the user address, or NULL on failure.
 * IRQL <= APC_LEVEL, in the context of the process that gets the mapping.
 */
PVOID hostmic_map_user(PMDL mdl, MEMORY_CACHING_TYPE cache)
{
    PVOID address = NULL;
    __try {
        address = MmMapLockedPagesSpecifyCache(mdl, UserMode, cache, NULL, FALSE,
                                               NormalPagePriority);
    } __except (EXCEPTION_EXECUTE_HANDLER) {
        address = NULL;
    }
    return address;
}
