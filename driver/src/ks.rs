//! Kernel Streaming helpers: static descriptor tables and property replies.

use core::ptr;

use hostmic_sys::{
    KSIDENTIFIER, KSIDENTIFIER__bindgen_ty_1, KSIDENTIFIER__bindgen_ty_1__bindgen_ty_1,
    KSMULTIPLE_ITEM, KSPROPERTY_DESCRIPTION, PCPROPERTY_REQUEST, guid,
};
use wdk_sys::{NTSTATUS, STATUS_BUFFER_OVERFLOW, STATUS_BUFFER_TOO_SMALL, STATUS_SUCCESS, ULONG};

/// `PCFILTER_NODE` (`portcls.h`): in a connection, "a pin of the filter" rather than of a
/// node.
pub const PCFILTER_NODE: ULONG = ULONG::MAX;

/// Static data handed to PortCls by pointer: filter, pin, node and property tables.
///
/// The descriptor structs hold raw pointers (to GUIDs, to other tables), so Rust does not
/// consider them shareable between threads; these tables are never written, by the driver or
/// by PortCls, which only reads them.
#[repr(transparent)]
pub struct Shared<T>(pub T);

// SAFETY: a `Shared` value is only ever in an immutable `static`, and the pointers it holds
// point to other immutable statics: every access, from any thread, is a read.
unsafe impl<T> Sync for Shared<T> {}

/// `size_of::<T>()` as a `ULONG`, checked at compile time.
///
/// # Panics
///
/// When `T` is larger than `ULONG::MAX` bytes; the driver only calls it in constants, where
/// that is a compile-time error.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    reason = "checked against ULONG::MAX just before"
)]
pub const fn size_ulong<T>() -> ULONG {
    let size = size_of::<T>();
    assert!(size <= ULONG::MAX as usize);
    size as ULONG
}

/// Replies to a `KSPROPERTY_TYPE_BASICSUPPORT` request with the access flags `flags`, as
/// a full `KSPROPERTY_DESCRIPTION` when the buffer holds one, else as a `ULONG` (Microsoft's
/// `PropertyHandler_BasicSupport`, without a property type).
pub fn basic_support(request: &mut PCPROPERTY_REQUEST, flags: ULONG) -> NTSTATUS {
    if request.Value.is_null() {
        request.ValueSize = 0;
        return STATUS_BUFFER_TOO_SMALL;
    }
    if request.ValueSize >= size_ulong::<KSPROPERTY_DESCRIPTION>() {
        let description = KSPROPERTY_DESCRIPTION {
            AccessFlags: flags,
            DescriptionSize: size_ulong::<KSPROPERTY_DESCRIPTION>(),
            PropTypeSet: KSIDENTIFIER {
                __bindgen_anon_1: KSIDENTIFIER__bindgen_ty_1 {
                    __bindgen_anon_1: KSIDENTIFIER__bindgen_ty_1__bindgen_ty_1 {
                        Set: guid::GUID_NULL,
                        Id: 0,
                        Flags: 0,
                    },
                },
            },
            MembersListCount: 0,
            Reserved: 0,
        };
        // SAFETY: PortCls's value buffer holds `ValueSize` bytes, checked above.
        unsafe {
            request
                .Value
                .cast::<KSPROPERTY_DESCRIPTION>()
                .write_unaligned(description);
        };
        request.ValueSize = size_ulong::<KSPROPERTY_DESCRIPTION>();
        STATUS_SUCCESS
    } else if request.ValueSize >= size_ulong::<ULONG>() {
        // SAFETY: PortCls's value buffer holds `ValueSize` bytes, checked above.
        unsafe { request.Value.cast::<ULONG>().write_unaligned(flags) };
        request.ValueSize = size_ulong::<ULONG>();
        STATUS_SUCCESS
    } else {
        request.ValueSize = 0;
        STATUS_BUFFER_TOO_SMALL
    }
}

/// Replies to a GET request with a `KSMULTIPLE_ITEM` header followed by one `T`, the shape of
/// the jack properties. A zero-sized buffer asks for the size (`STATUS_BUFFER_OVERFLOW`).
pub fn reply_multiple_item<T>(request: &mut PCPROPERTY_REQUEST, item: T) -> NTSTATUS {
    let needed = size_ulong::<KSMULTIPLE_ITEM>() + size_ulong::<T>();
    if request.ValueSize == 0 {
        request.ValueSize = needed;
        return STATUS_BUFFER_OVERFLOW;
    }
    if request.ValueSize < needed || request.Value.is_null() {
        return STATUS_BUFFER_TOO_SMALL;
    }
    let header = request.Value.cast::<KSMULTIPLE_ITEM>();
    // SAFETY: PortCls's value buffer holds `ValueSize >= needed` bytes, checked above.
    unsafe {
        header.write_unaligned(KSMULTIPLE_ITEM {
            Size: needed,
            Count: 1,
        });
    };
    let value = header.wrapping_add(1).cast::<T>();
    // SAFETY: the item follows the header within the same `needed` bytes.
    unsafe { value.write_unaligned(item) };
    request.ValueSize = needed;
    STATUS_SUCCESS
}

/// Reads the pin number at the start of a property's instance data (the `KSP_PIN` that follows
/// the `KSPROPERTY` header), or `None` when the instance is too short.
#[must_use]
pub fn instance_pin(request: &PCPROPERTY_REQUEST) -> Option<ULONG> {
    if request.Instance.is_null() || request.InstanceSize < size_ulong::<ULONG>() {
        return None;
    }
    // SAFETY: PortCls's instance buffer holds `InstanceSize` bytes, checked above.
    Some(unsafe { ptr::read_unaligned(request.Instance.cast::<ULONG>()) })
}
