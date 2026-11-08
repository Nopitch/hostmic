//! Fast I/O: kept away from the host interface.
//!
//! Besides IRPs, the I/O manager may call a driver's fast I/O routines
//! (`DRIVER_OBJECT::FastIoDispatch`) for any file object of its devices: device control, read
//! and write requests first try them. If PortCls (Kernel Streaming) installs some, they expect
//! a Kernel Streaming file object, which the host interface's are not.
//!
//! So the driver replaces the table with its own copy: device control, read and write go
//! through wrappers that decline (return `FALSE`, so the I/O manager builds an IRP) for the host
//! interface and call PortCls's routine otherwise; every other entry is dropped, which only
//! means IRPs instead of the fast path (none of them matters to an audio adapter).

use core::{
    cell::UnsafeCell,
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

use wdk::println;
use wdk_sys::{
    BOOLEAN, DRIVER_OBJECT, FAST_IO_DISPATCH, PDEVICE_OBJECT, PFILE_OBJECT, PIO_STATUS_BLOCK,
    PLARGE_INTEGER, PVOID, ULONG,
};

use crate::host_device;
use crate::ks::size_ulong;

/// The driver's fast I/O table, written once in `DriverEntry`.
struct Table(UnsafeCell<FAST_IO_DISPATCH>);

// SAFETY: written once by `install`, in `DriverEntry`, before the I/O manager can read it;
// read-only afterwards.
unsafe impl Sync for Table {}

// SAFETY: an all-zero `FAST_IO_DISPATCH` is valid: no routine.
static TABLE: Table = Table(UnsafeCell::new(unsafe { core::mem::zeroed() }));

/// PortCls's table, which the wrappers call for the adapter's file objects.
static PORTCLS: AtomicPtr<FAST_IO_DISPATCH> = AtomicPtr::new(ptr::null_mut());

/// Replaces PortCls's fast I/O table, if it installed one, with the driver's.
///
/// # Safety
///
/// Called once from `DriverEntry`, after `PcInitializeAdapterDriver`, before any I/O.
pub unsafe fn install(driver: &mut DRIVER_OBJECT) {
    let original = driver.FastIoDispatch;
    if original.is_null() {
        println!("hostmic: no fast I/O table");
        return;
    }
    // SAFETY: PortCls's table lives as long as PortCls is loaded, which outlasts this driver.
    let portcls = unsafe { &*original };
    println!(
        "hostmic: PortCls fast I/O: device control {}, read {}, write {}",
        portcls.FastIoDeviceControl.is_some(),
        portcls.FastIoRead.is_some(),
        portcls.FastIoWrite.is_some()
    );
    PORTCLS.store(original, Ordering::Release);
    let table = FAST_IO_DISPATCH {
        SizeOfFastIoDispatch: size_ulong::<FAST_IO_DISPATCH>(),
        FastIoDeviceControl: portcls
            .FastIoDeviceControl
            .map(|_| device_control as unsafe extern "C" fn(_, _, _, _, _, _, _, _, _) -> _),
        FastIoRead: portcls
            .FastIoRead
            .map(|_| read as unsafe extern "C" fn(_, _, _, _, _, _, _, _) -> _),
        FastIoWrite: portcls
            .FastIoWrite
            .map(|_| write as unsafe extern "C" fn(_, _, _, _, _, _, _, _) -> _),
        // SAFETY: every other entry absent.
        ..unsafe { core::mem::zeroed() }
    };
    // SAFETY: the caller's contract: no reader yet.
    unsafe { TABLE.0.get().write(table) };
    driver.FastIoDispatch = TABLE.0.get();
}

/// PortCls's table.
fn portcls<'a>() -> Option<&'a FAST_IO_DISPATCH> {
    let table = PORTCLS.load(Ordering::Acquire);
    // SAFETY: null, or PortCls's table, which lives as long as PortCls.
    unsafe { table.as_ref() }
}

/// Fast I/O device control: declined for the host interface.
unsafe extern "C" fn device_control(
    file: PFILE_OBJECT,
    wait: BOOLEAN,
    input: PVOID,
    input_length: ULONG,
    output: PVOID,
    output_length: ULONG,
    code: ULONG,
    status: PIO_STATUS_BLOCK,
    device: PDEVICE_OBJECT,
) -> BOOLEAN {
    if host_device::owns(device) {
        return 0;
    }
    match portcls().and_then(|table| table.FastIoDeviceControl) {
        // SAFETY: forwards the I/O manager's call, for one of the adapter's file objects, to
        // the routine PortCls installed for it.
        Some(routine) => unsafe {
            routine(
                file,
                wait,
                input,
                input_length,
                output,
                output_length,
                code,
                status,
                device,
            )
        },
        None => 0,
    }
}

/// Fast I/O read: declined for the host interface.
unsafe extern "C" fn read(
    file: PFILE_OBJECT,
    offset: PLARGE_INTEGER,
    length: ULONG,
    wait: BOOLEAN,
    key: ULONG,
    buffer: PVOID,
    status: PIO_STATUS_BLOCK,
    device: PDEVICE_OBJECT,
) -> BOOLEAN {
    if host_device::owns(device) {
        return 0;
    }
    match portcls().and_then(|table| table.FastIoRead) {
        // SAFETY: as in `device_control`.
        Some(routine) => unsafe {
            routine(file, offset, length, wait, key, buffer, status, device)
        },
        None => 0,
    }
}

/// Fast I/O write: declined for the host interface.
unsafe extern "C" fn write(
    file: PFILE_OBJECT,
    offset: PLARGE_INTEGER,
    length: ULONG,
    wait: BOOLEAN,
    key: ULONG,
    buffer: PVOID,
    status: PIO_STATUS_BLOCK,
    device: PDEVICE_OBJECT,
) -> BOOLEAN {
    if host_device::owns(device) {
        return 0;
    }
    match portcls().and_then(|table| table.FastIoWrite) {
        // SAFETY: as in `device_control`.
        Some(routine) => unsafe {
            routine(file, offset, length, wait, key, buffer, status, device)
        },
        None => 0,
    }
}
