//! # hostmic driver
//!
//! A PortCls audio adapter, written in Rust, that will expose one capture endpoint (a
//! microphone) fed by a user-mode host. It ports the capture path of Microsoft's
//! `simpleaudiosample` (Copyright (c) Microsoft Corporation, MS-PL); PLAN.md lists where it
//! deliberately departs from it.
//!
//! Milestone M3: the endpoint "Microphone (hostmic)", whose capture stream the driver fills
//! itself with a 1 kHz test tone; M4 hands the buffer and the clock to a user-mode host,
//! through a host interface of its own.
//!
//! - [`com`]: COM-style objects and interface references.
//! - [`ks`]: Kernel Streaming descriptor and property helpers.
//! - [`adapter`]: the PnP device and its subdevices.
//! - [`host_device`]: the host interface's device object, a child of the adapter.
//! - [`host_link`]: what the stream and the host interface share.
//! - [`fast_io`]: the fast I/O table, kept away from the host interface.
//! - [`topology`]: the topology miniport.
//! - [`wave`]: the WaveRT miniport.
//! - [`stream`]: the capture stream and its timer.

#![cfg_attr(not(test), no_std)]

extern crate alloc;
#[cfg(not(test))]
extern crate wdk_panic;

pub mod adapter;
pub mod com;
pub mod fast_io;
pub mod host_device;
pub mod host_link;
pub mod ks;
pub mod stream;
pub mod topology;
pub mod wave;

use core::{
    ffi::c_void,
    sync::atomic::{AtomicPtr, Ordering},
};

use hostmic_sys::PcInitializeAdapterDriver;
use wdk::println;
#[cfg(not(test))]
use wdk_alloc::WdkAllocator;
use wdk_sys::{
    DEVICE_OBJECT, DRIVER_OBJECT, IRP, IRP_MJ_MAXIMUM_FUNCTION, IRP_MJ_PNP, NT_SUCCESS, NTSTATUS,
    PCUNICODE_STRING, STATUS_INVALID_DEVICE_REQUEST,
};

#[cfg(not(test))]
#[global_allocator]
static GLOBAL_ALLOCATOR: WdkAllocator = WdkAllocator;

/// The unload routine `PcInitializeAdapterDriver` installed, called by [`driver_unload`].
static PORTCLS_UNLOAD: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());

/// The driver's entry point, called once by the kernel when the driver is loaded.
///
/// # Safety
///
/// Called by the kernel only, with a valid driver object and registry path.
// SAFETY: `DriverEntry` is the symbol name the kernel looks for; no other function in this
// crate exports it.
#[cfg_attr(not(test), unsafe(export_name = "DriverEntry"))]
pub unsafe extern "system" fn driver_entry(
    driver: &mut DRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    println!("hostmic: DriverEntry");

    // PortCls fills the dispatch table, the AddDevice routine and the unload routine.
    // SAFETY: the driver object and registry path come from the kernel and are valid;
    // `add_device` matches `DRIVER_ADD_DEVICE`. PortCls does not write the registry path.
    let status = unsafe {
        PcInitializeAdapterDriver(driver, registry_path.cast_mut(), Some(adapter::add_device))
    };
    println!("hostmic: PcInitializeAdapterDriver returned {status:#010x}");
    if !NT_SUCCESS(status) {
        return status;
    }

    // Every request goes through `dispatch`, which sends the host interface's to its own
    // handlers and the adapter's to the routines PortCls installed.
    for (major, routine) in driver.MajorFunction.iter_mut().enumerate() {
        let original = routine.map_or(core::ptr::null_mut(), |f| f as *mut c_void);
        PORTCLS_DISPATCH[major].store(original, Ordering::Release);
        *routine = Some(dispatch);
    }
    // SAFETY: once, after PortCls installed its routines, before any I/O.
    unsafe { fast_io::install(driver) };
    // The driver's unload routine is installed whatever PortCls left there, so that the driver
    // stays unloadable; it chains to PortCls's when there is one (Microsoft's sample does the
    // same).
    if let Some(unload) = driver.DriverUnload {
        PORTCLS_UNLOAD.store(unload as *mut c_void, Ordering::Release);
    }
    driver.DriverUnload = Some(driver_unload);
    status
}

/// The dispatch routines `PcInitializeAdapterDriver` installed, by major function, called by
/// [`dispatch`] for the adapter's requests (null where it installed none).
static PORTCLS_DISPATCH: [AtomicPtr<c_void>; IRP_MJ_MAXIMUM_FUNCTION as usize + 1] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; IRP_MJ_MAXIMUM_FUNCTION as usize + 1];

/// The driver's dispatch routine for every major function.
///
/// The driver owns two kinds of device objects: the adapter's FDO, whose requests belong to
/// PortCls (PnP through [`adapter::dispatch_pnp`] first), and the host interface's PDO, which
/// PortCls knows nothing about and must never see: its extension is not PortCls's.
///
/// # Safety
///
/// Called by the I/O manager with a device object of this driver and an IRP for it.
unsafe extern "C" fn dispatch(device: *mut DEVICE_OBJECT, irp: *mut IRP) -> NTSTATUS {
    if host_device::owns(device) {
        // SAFETY: a request for the host interface's PDO.
        return unsafe { host_device::dispatch(device, irp) };
    }
    // SAFETY: an IRP being dispatched to this driver.
    let stack = unsafe { adapter::current_stack_location(irp) };
    // SAFETY: an IRP being dispatched has a valid current stack location.
    let major = usize::from(unsafe { (*stack).MajorFunction });
    if major == IRP_MJ_PNP as usize {
        // SAFETY: a PnP request for the adapter's FDO.
        return unsafe { adapter::dispatch_pnp(device, irp) };
    }
    let original = PORTCLS_DISPATCH
        .get(major)
        .map_or(core::ptr::null_mut(), |slot| slot.load(Ordering::Acquire));
    if original.is_null() {
        // SAFETY: a request owned by this driver, not completed yet.
        return unsafe { host_device::complete(irp, STATUS_INVALID_DEVICE_REQUEST, 0) };
    }
    // SAFETY: `original` was stored from a `DRIVER_DISPATCH` function pointer in
    // `driver_entry`.
    let original = unsafe {
        core::mem::transmute::<
            *mut c_void,
            unsafe extern "C" fn(*mut DEVICE_OBJECT, *mut IRP) -> NTSTATUS,
        >(original)
    };
    // SAFETY: forwards the I/O manager's call to the routine PortCls installed for it.
    unsafe { original(device, irp) }
}

/// The driver's unload routine: logs, then runs PortCls's.
unsafe extern "C" fn driver_unload(driver: *mut DRIVER_OBJECT) {
    println!("hostmic: driver unloaded");
    let unload = PORTCLS_UNLOAD.load(Ordering::Acquire);
    if !unload.is_null() {
        // SAFETY: `unload` was stored from a `DRIVER_UNLOAD` function pointer in
        // `driver_entry`, before this routine was installed.
        let unload = unsafe {
            core::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut DRIVER_OBJECT)>(unload)
        };
        // SAFETY: forwards the kernel's call with the same driver object.
        unsafe { unload(driver) };
    }
}
