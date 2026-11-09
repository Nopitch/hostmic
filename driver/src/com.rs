//! COM-style objects, the way PortCls calls a miniport and the way the miniport calls PortCls.
//!
//! PortCls interfaces are C++ abstract classes: an interface pointer points to a field that
//! holds a pointer to a table of function pointers (the vtable), whose first three entries are
//! `IUnknown`'s `QueryInterface`, `AddRef` and `Release`. In C (and in `hostmic-sys`) an
//! interface is a struct `{ lpVtbl }`, and each method takes the interface pointer as its first
//! argument, `This`.
//!
//! The driver's own objects are `#[repr(C)]` structs with one `&'static` vtable field per
//! interface chain (offset 0 for the primary one) and an atomic reference count
//! ([`ComObject`]). They live in non-paged pool; the last `Release` drops and frees them. A
//! method receives the address of the field it was called through and finds its object by
//! subtracting that field's offset, a compile-time constant of the method's instantiation.
//!
//! Interfaces received from PortCls (the ports) are held by [`Unknown`], which releases its
//! reference when dropped.

use core::{
    alloc::Layout,
    ffi::c_void,
    ptr::{self, NonNull},
    sync::atomic::{AtomicU32, Ordering, fence},
};

use hostmic_sys::{IUnknown, guid};
use wdk_sys::{GUID, NTSTATUS, PVOID, STATUS_INVALID_PARAMETER, STATUS_SUCCESS, ULONG};

/// COM's `IID_IUnknown` (`ksguid.lib`), `00000000-0000-0000-C000-000000000046`. PortCls's
/// `punknown.h` defines a different value (`hostmic_sys::guid::IID_IUnknown`); callers may use
/// either, so both are accepted.
pub const IID_IUNKNOWN_COM: GUID = GUID {
    Data1: 0,
    Data2: 0,
    Data3: 0,
    Data4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46],
};

/// Formats a GUID the usual way, for the debug output.
pub struct GuidDisplay<'a>(pub &'a GUID);

impl core::fmt::Display for GuidDisplay<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let g = self.0;
        let d = g.Data4;
        write!(
            f,
            "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
            g.Data1, g.Data2, g.Data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
        )
    }
}

/// Whether two GUIDs are equal.
#[must_use]
pub const fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a.Data1 == b.Data1
        && a.Data2 == b.Data2
        && a.Data3 == b.Data3
        && u64::from_ne_bytes(a.Data4) == u64::from_ne_bytes(b.Data4)
}

/// A driver object that PortCls holds through interface pointers.
///
/// # Safety
///
/// The implementor is `#[repr(C)]`, its field at offset 0 holds a vtable pointer, and each
/// offset in [`ComObject::INTERFACES`] is the offset of a field holding a vtable pointer. The
/// `IUnknown` entries of the vtable at offset `n` are [`query_interface`], [`add_ref`] and
/// [`release`] instantiated for this type and `n`. Its methods may run on several threads at
/// once, hence `Sync`.
pub unsafe trait ComObject: Sized + Sync {
    /// The interfaces the object answers to besides `IUnknown` (always offset 0), with the
    /// offset of the vtable field that implements each.
    const INTERFACES: &'static [(GUID, usize)];

    /// The reference count.
    fn references(&self) -> &AtomicU32;
}

/// Moves `object`, whose reference count must be 1, into non-paged pool and returns the
/// pointer that owns that reference, or `None` when the pool is exhausted.
pub fn into_unknown<T: ComObject>(object: T) -> Option<Unknown> {
    const {
        assert!(
            size_of::<T>() > 0,
            "a COM object holds at least a vtable pointer"
        );
    };
    debug_assert_eq!(object.references().load(Ordering::Relaxed), 1);
    let layout = Layout::new::<T>();
    // SAFETY: the layout is not zero-sized (checked above).
    let raw = unsafe { alloc::alloc::alloc(layout) }.cast::<T>();
    let raw = NonNull::new(raw)?;
    // SAFETY: `raw` is a fresh allocation with `T`'s layout.
    unsafe { raw.write(object) };
    // SAFETY: the object's field at offset 0 holds a vtable pointer whose first entries are
    // `IUnknown`'s (`ComObject`'s contract): it is an `IUnknown`, and it owns one reference.
    unsafe { Unknown::from_raw(raw.as_ptr().cast()) }
}

/// The object that an interface pointer of offset `OFFSET` belongs to.
///
/// # Safety
///
/// `this` points to the field at offset `OFFSET` of a live `T`, and the caller holds a
/// reference to it.
pub unsafe fn object<'a, T: ComObject, const OFFSET: usize>(this: *mut c_void) -> &'a T {
    let object = this.wrapping_byte_sub(OFFSET).cast::<T>();
    // SAFETY: the caller's contract: `object` is the start of a live `T`, which lives at least
    // as long as the caller's reference.
    unsafe { &*object }
}

/// `IUnknown::QueryInterface` for the vtable at offset `OFFSET` of a `T`.
///
/// Returns `STATUS_SUCCESS` and a new reference for `IUnknown` (either value) and the
/// interfaces in `T::INTERFACES`, `STATUS_INVALID_PARAMETER` and a null pointer otherwise.
///
/// # Safety
///
/// Called through a vtable of a live `T` (see [`ComObject`]); `interface_id` is null or valid,
/// `interface` is null or writable.
pub unsafe extern "C" fn query_interface<T: ComObject, const OFFSET: usize>(
    this: *mut c_void,
    interface_id: *const GUID,
    interface: *mut PVOID,
) -> NTSTATUS {
    if interface.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: the caller passes a valid IID or null.
    let iid = unsafe { interface_id.as_ref() };
    let found = match iid {
        Some(iid) if guid_eq(iid, &guid::IID_IUnknown) || guid_eq(iid, &IID_IUNKNOWN_COM) => {
            Some(0)
        }
        Some(iid) => T::INTERFACES
            .iter()
            .find(|(candidate, _)| guid_eq(candidate, iid))
            .map(|&(_, offset)| offset),
        None => None,
    };
    let base = this.wrapping_byte_sub(OFFSET);
    let result = if let Some(offset) = found {
        // SAFETY: `base` is the live `T` that `this` belongs to.
        unsafe { object::<T, 0>(base) }
            .references()
            .fetch_add(1, Ordering::Relaxed);
        base.wrapping_byte_add(offset)
    } else {
        // Optional interfaces PortCls probes for (power, signal processing modes…); logged
        // so that a missing required one shows up in the debug output.
        if let Some(iid) = iid {
            wdk::println!(
                "hostmic: {} has no interface {}",
                core::any::type_name::<T>(),
                GuidDisplay(iid)
            );
        }
        ptr::null_mut()
    };
    // SAFETY: `interface` is writable (caller's contract) and not null (checked above).
    unsafe { interface.write(result) };
    if found.is_some() {
        STATUS_SUCCESS
    } else {
        STATUS_INVALID_PARAMETER
    }
}

/// `IUnknown::AddRef` for the vtable at offset `OFFSET` of a `T`.
///
/// # Safety
///
/// Called through a vtable of a live `T` by a holder of a reference.
pub unsafe extern "C" fn add_ref<T: ComObject, const OFFSET: usize>(this: *mut c_void) -> ULONG {
    // SAFETY: the caller's contract.
    let object = unsafe { object::<T, OFFSET>(this) };
    object.references().fetch_add(1, Ordering::Relaxed) + 1
}

/// `IUnknown::Release` for the vtable at offset `OFFSET` of a `T`: the last one drops the
/// object and frees its memory.
///
/// # Safety
///
/// Called through a vtable of a live `T` by a holder of a reference, which it gives up. The
/// last release runs `T`'s destructor, at the caller's IRQL.
pub unsafe extern "C" fn release<T: ComObject, const OFFSET: usize>(this: *mut c_void) -> ULONG {
    let raw = this.wrapping_byte_sub(OFFSET).cast::<T>();
    // SAFETY: the caller's contract; the reference is not used after the decrement unless it
    // was the last one.
    let previous = unsafe { object::<T, OFFSET>(this) }
        .references()
        .fetch_sub(1, Ordering::Release);
    if previous == 1 {
        // Every other holder's last access happens before the destruction (`Release` above,
        // `Acquire` here), as in `Arc`.
        fence(Ordering::Acquire);
        // SAFETY: that was the last reference: nobody else can reach the object any more.
        unsafe { ptr::drop_in_place(raw) };
        // SAFETY: `raw` was allocated by `into_unknown` with this layout.
        unsafe { alloc::alloc::dealloc(raw.cast(), Layout::new::<T>()) };
    }
    // A release without a reference is a caller bug; do not turn it into a bugcheck here.
    previous.saturating_sub(1)
}

/// A counted reference to an interface, released when dropped.
#[derive(Debug)]
pub struct Unknown(NonNull<IUnknown>);

// SAFETY: COM reference counting is thread-safe and PortCls interfaces can be called from any
// thread; a reference can move to another thread.
unsafe impl Send for Unknown {}

/// `QueryInterface` on an interface pointer the caller only borrows (no reference taken or
/// released on it): a new reference to the interface `iid`, or `None`.
///
/// # Safety
///
/// `raw` is null or a valid interface pointer, alive for the call.
#[must_use]
pub unsafe fn query_borrowed(raw: *mut IUnknown, iid: &GUID) -> Option<Unknown> {
    // Not dropped: the borrowed reference is not ours to release.
    // SAFETY: the caller's contract; `ManuallyDrop` keeps `from_raw`'s ownership unused.
    let borrowed = core::mem::ManuallyDrop::new(unsafe { Unknown::from_raw(raw) }?);
    borrowed.query(iid)
}

impl Unknown {
    /// Takes ownership of one reference.
    ///
    /// # Safety
    ///
    /// `raw` is null or a valid interface pointer (whose vtable starts with `IUnknown`'s
    /// methods) carrying a reference that the caller gives up.
    #[must_use]
    pub unsafe fn from_raw(raw: *mut IUnknown) -> Option<Self> {
        NonNull::new(raw).map(Self)
    }

    /// The interface pointer, still owned by `self`.
    #[must_use]
    pub fn as_ptr(&self) -> *mut IUnknown {
        self.0.as_ptr()
    }

    /// Gives up ownership: the reference now belongs to whoever receives the pointer.
    #[must_use]
    pub fn into_raw(self) -> *mut IUnknown {
        let raw = self.0.as_ptr();
        core::mem::forget(self);
        raw
    }

    /// `QueryInterface`: a new reference to the interface `iid` of the same object, or `None`
    /// when the object does not implement it.
    #[must_use]
    pub fn query(&self, iid: &GUID) -> Option<Self> {
        let this = self.0.as_ptr();
        // SAFETY: `this` is a valid interface pointer (`from_raw`'s contract).
        let vtable = unsafe { (*this).lpVtbl };
        // SAFETY: an interface's vtable is valid for the interface's lifetime.
        let query_interface = unsafe { (*vtable).QueryInterface }?;
        let mut interface: PVOID = ptr::null_mut();
        // SAFETY: valid interface, IID and out-parameter.
        let status = unsafe { query_interface(this, iid, &raw mut interface) };
        if status != STATUS_SUCCESS {
            return None;
        }
        // SAFETY: a successful `QueryInterface` returns a new reference to an interface whose
        // vtable starts with `IUnknown`'s methods.
        unsafe { Self::from_raw(interface.cast()) }
    }
}

impl Drop for Unknown {
    fn drop(&mut self) {
        let this = self.0.as_ptr();
        // SAFETY: `this` is a valid interface pointer (`from_raw`'s contract).
        let vtable = unsafe { (*this).lpVtbl };
        // SAFETY: an interface's vtable is valid for the interface's lifetime.
        let release = unsafe { (*vtable).Release };
        if let Some(release) = release {
            // SAFETY: gives up the reference this value owns; `this` is not used afterwards.
            unsafe { release(this) };
        }
    }
}
