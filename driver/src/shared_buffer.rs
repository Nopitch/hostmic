//! The buffer shared by the host and the audio engine (PLAN.md, M4).
//!
//! WaveRT lets the miniport allocate each stream's buffer and frees it when the stream closes;
//! a host mapping that buffer would lose it under its feet. Instead the adapter owns one buffer
//! for its whole life, of [`MAX_BUFFER_BYTES`] rounded up to whole pages:
//!
//! - its pages come from `MmAllocatePagesForMdlEx`, zeroed, so that no kernel data ever reaches
//!   user mode, and are mapped once in system space;
//! - each stream gets its own partial MDL over the start of the buffer
//!   ([`SharedBuffer::stream_mdl`]), which `AllocateAudioBuffer` returns to PortCls, which
//!   maps it into the audio engine's process;
//! - the attached host gets a user-mode mapping of the whole buffer
//!   ([`SharedBuffer::map_user`]), made and undone in its own process.
//!
//! The adapter, each stream buffer and the host mapping hold a [`BufferRef`]; the pages are
//! freed with the last one, so that a stream or a host outliving the adapter's removal never
//! reads freed pages.
//!
//! The driver writes into the buffer (zeroes, and until the host advances the position, the
//! test tone) and never reads it: what the host writes is opaque data for the audio engine.

use core::{
    alloc::Layout,
    ffi::c_void,
    ptr::{self, NonNull},
    sync::atomic::{AtomicU32, Ordering, fence},
};

use hostmic_driver_core::host::MAX_BUFFER_BYTES;
use hostmic_sys::hostmic_map_user;
use wdk_sys::{
    _MEMORY_CACHING_TYPE, _MM_PAGE_PRIORITY, MDL, MM_ALLOCATE_FULLY_REQUIRED, MdlMappingNoExecute,
    PAGE_SIZE, PHYSICAL_ADDRESS,
    ntddk::{
        ExFreePool, IoAllocateMdl, IoBuildPartialMdl, IoFreeMdl, MmAllocatePagesForMdlEx,
        MmFreePagesFromMdl, MmMapLockedPagesSpecifyCache, MmUnmapLockedPages,
    },
};

/// `KPROCESSOR_MODE` `KernelMode`.
const KERNEL_MODE: i8 = 0;

/// The shared buffer's size: [`MAX_BUFFER_BYTES`] rounded up to whole pages, so that a user
/// mapping exposes nothing but the buffer.
const BYTES: usize = (MAX_BUFFER_BYTES as usize).div_ceil(PAGE_SIZE as usize) * PAGE_SIZE as usize;

/// The pages and their system mapping, with a reference count.
struct SharedBuffer {
    references: AtomicU32,
    /// The pages, from `MmAllocatePagesForMdlEx`: [`BYTES`] bytes.
    mdl: NonNull<MDL>,
    /// Their mapping in system space.
    system: NonNull<u8>,
}

/// A counted reference to the shared buffer. `PASSIVE_LEVEL` for every operation, including
/// drop.
pub struct BufferRef(NonNull<SharedBuffer>);

// SAFETY: the buffer's fields never change after creation and its count is atomic; the pages
// are shared memory that the driver only writes through raw pointers.
unsafe impl Send for BufferRef {}
// SAFETY: as above.
unsafe impl Sync for BufferRef {}

impl BufferRef {
    /// Allocates and maps a zeroed buffer of [`BYTES`] bytes; `None` when memory is short.
    /// `PASSIVE_LEVEL`.
    #[must_use]
    pub fn allocate() -> Option<Self> {
        let none = PHYSICAL_ADDRESS { QuadPart: 0 };
        let any = PHYSICAL_ADDRESS { QuadPart: -1 };
        // SAFETY: PASSIVE_LEVEL; any physical address, no skip, all pages or none, zeroed (the
        // default: no `MM_DONT_ZERO_ALLOCATION`).
        let mdl = unsafe {
            MmAllocatePagesForMdlEx(
                none,
                any,
                none,
                BYTES as u64,
                _MEMORY_CACHING_TYPE::MmCached,
                MM_ALLOCATE_FULLY_REQUIRED,
            )
        };
        let mdl = NonNull::new(mdl)?;
        // SAFETY: an MDL of locked pages just allocated; kernel mode, no bug check on failure
        // (null instead), never executable.
        let system = unsafe {
            MmMapLockedPagesSpecifyCache(
                mdl.as_ptr(),
                KERNEL_MODE,
                _MEMORY_CACHING_TYPE::MmCached,
                ptr::null_mut(),
                0,
                _MM_PAGE_PRIORITY::NormalPagePriority as u32 | MdlMappingNoExecute,
            )
        };
        let Some(system) = NonNull::new(system.cast::<u8>()) else {
            // SAFETY: the pages just allocated, not mapped.
            unsafe { free_pages(mdl) };
            return None;
        };
        let layout = Layout::new::<SharedBuffer>();
        #[allow(
            clippy::cast_ptr_alignment,
            reason = "allocated with `SharedBuffer`'s layout, so aligned for it"
        )]
        // SAFETY: a non-zero-sized layout.
        let raw = unsafe { alloc::alloc::alloc(layout) }.cast::<SharedBuffer>();
        let Some(raw) = NonNull::new(raw) else {
            // SAFETY: the mapping made above, of these pages.
            unsafe { MmUnmapLockedPages(system.as_ptr().cast(), mdl.as_ptr()) };
            // SAFETY: the pages allocated above, no longer mapped.
            unsafe { free_pages(mdl) };
            return None;
        };
        // SAFETY: a fresh allocation with `SharedBuffer`'s layout.
        unsafe {
            raw.write(SharedBuffer {
                references: AtomicU32::new(1),
                mdl,
                system,
            });
        };
        Some(Self(raw))
    }

    fn get(&self) -> &SharedBuffer {
        // SAFETY: alive while a reference exists.
        unsafe { self.0.as_ref() }
    }

    /// Gives up the reference as a raw pointer, for an atomic slot.
    #[must_use]
    pub fn into_raw(self) -> *mut c_void {
        let raw = self.0.as_ptr().cast();
        core::mem::forget(self);
        raw
    }

    /// Takes back a reference given up by [`BufferRef::into_raw`].
    ///
    /// # Safety
    ///
    /// `raw` came from `into_raw` and is taken back once.
    #[must_use]
    pub unsafe fn from_raw(raw: *mut c_void) -> Option<Self> {
        NonNull::new(raw.cast()).map(Self)
    }

    /// The buffer in system space: [`BufferRef::size`] bytes, valid while `self` lives.
    #[must_use]
    pub fn system(&self) -> *mut u8 {
        self.get().system.as_ptr()
    }

    /// The buffer's size in bytes (whole pages).
    #[allow(
        clippy::unused_self,
        reason = "a property of the buffer, constant for now"
    )]
    #[must_use]
    pub fn size(&self) -> usize {
        BYTES
    }

    /// Writes zeroes over the first `bytes` bytes (at most the whole buffer), rounded down to
    /// whole 32-bit words.
    ///
    /// The pages are shared memory: the host and the audio engine access them through their
    /// own mappings, and the stream's timer may write the test tone meanwhile. Like a device's
    /// memory, they are only ever written through volatile accesses of aligned words, never
    /// through a Rust reference.
    pub fn zero(&self, bytes: usize) {
        #[allow(clippy::cast_ptr_alignment, reason = "the mapping is page-aligned")]
        let words = self.system().cast::<u32>();
        for index in 0..bytes.min(BYTES) / 4 {
            let word = words.wrapping_add(index);
            // SAFETY: within the system mapping (`BYTES` bytes), aligned (page-aligned base).
            unsafe { word.write_volatile(0) };
        }
    }

    /// A new MDL over the buffer's first `bytes` bytes, for a stream; freed with
    /// [`BufferRef::free_stream_mdl`] while `self` lives. `None` when `bytes` is 0 or exceeds
    /// the buffer, or when memory is short.
    #[must_use]
    pub fn stream_mdl(&self, bytes: u32) -> Option<NonNull<MDL>> {
        if bytes == 0 || bytes as usize > BYTES {
            return None;
        }
        let source = self.get().mdl;
        // `MmGetMdlVirtualAddress`: the start of the range the source MDL describes. Partial
        // MDLs are placed relative to it.
        let base = {
            // SAFETY: the buffer's own MDL, never changed after allocation.
            let source = unsafe { source.as_ref() };
            source
                .StartVa
                .cast::<u8>()
                .wrapping_add(source.ByteOffset as usize)
                .cast::<c_void>()
        };
        // SAFETY: <= DISPATCH_LEVEL; an MDL sized for `bytes` bytes from `base`, not attached
        // to an IRP, no quota.
        let partial = NonNull::new(unsafe { IoAllocateMdl(base, bytes, 0, 0, ptr::null_mut()) })?;
        // SAFETY: the source's pages are locked (allocated by `MmAllocatePagesForMdlEx`);
        // `base` and `bytes` describe a subrange of it (checked above); `partial` is large
        // enough (allocated for that range).
        unsafe { IoBuildPartialMdl(source.as_ptr(), partial.as_ptr(), base, bytes) };
        Some(partial)
    }

    /// Frees an MDL from [`BufferRef::stream_mdl`]; its mappings are undone already.
    ///
    /// # Safety
    ///
    /// `mdl` came from `stream_mdl` on this buffer and is freed once.
    pub unsafe fn free_stream_mdl(&self, mdl: NonNull<MDL>) {
        // SAFETY: the caller's contract; `IoFreeMdl` also releases a system mapping the partial
        // MDL may have got of its own.
        unsafe { IoFreeMdl(mdl.as_ptr()) };
    }

    /// Maps the whole buffer into the current process, readable and writable, never
    /// executable; `None` when it cannot be mapped.
    ///
    /// # Safety
    ///
    /// `PASSIVE_LEVEL`, in the context of the process that gets the mapping, which must be
    /// undone by [`BufferRef::unmap_user`] in that process's context before the process ends
    /// and before `self` is dropped.
    #[must_use]
    pub unsafe fn map_user(&self) -> Option<NonNull<c_void>> {
        // SAFETY: an MDL of locked pages; the caller's contract.
        NonNull::new(unsafe {
            hostmic_map_user(self.get().mdl.as_ptr(), _MEMORY_CACHING_TYPE::MmCached)
        })
    }

    /// Undoes a mapping made by [`BufferRef::map_user`].
    ///
    /// # Safety
    ///
    /// `address` came from `map_user` on this buffer, in the current process's context (or
    /// one attached to), and is unmapped once; `PASSIVE_LEVEL`.
    pub unsafe fn unmap_user(&self, address: NonNull<c_void>) {
        // SAFETY: the caller's contract.
        unsafe { MmUnmapLockedPages(address.as_ptr(), self.get().mdl.as_ptr()) };
    }
}

impl Clone for BufferRef {
    fn clone(&self) -> Self {
        self.get().references.fetch_add(1, Ordering::Relaxed);
        Self(self.0)
    }
}

impl Drop for BufferRef {
    /// Frees the buffer with its last reference. `PASSIVE_LEVEL`.
    fn drop(&mut self) {
        if self.get().references.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        fence(Ordering::Acquire);
        let (mdl, system) = (self.get().mdl, self.get().system);
        // SAFETY: the system mapping made by `allocate`, of these pages; no reference is left,
        // so no user of it either.
        unsafe { MmUnmapLockedPages(system.as_ptr().cast(), mdl.as_ptr()) };
        // SAFETY: the pages allocated by `allocate`, no longer mapped anywhere: every stream
        // MDL and host mapping held a reference, released after their mappings were undone.
        unsafe { free_pages(mdl) };
        // SAFETY: allocated by `allocate` with this layout; nothing refers to it any more.
        unsafe { alloc::alloc::dealloc(self.0.as_ptr().cast(), Layout::new::<SharedBuffer>()) };
    }
}

/// Frees pages from `MmAllocatePagesForMdlEx` and their MDL.
///
/// # Safety
///
/// `mdl` came from `MmAllocatePagesForMdlEx`, its pages are mapped nowhere, and it is freed
/// once.
unsafe fn free_pages(mdl: NonNull<MDL>) {
    // SAFETY: the caller's contract.
    unsafe { MmFreePagesFromMdl(mdl.as_ptr()) };
    // SAFETY: the MDL structure itself, which `MmFreePagesFromMdl` leaves to the caller.
    unsafe { ExFreePool(mdl.as_ptr().cast()) };
}
