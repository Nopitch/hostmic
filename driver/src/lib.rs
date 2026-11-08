//! # hostmic driver
//!
//! A PortCls audio adapter, written in Rust, that will expose one capture endpoint (a
//! microphone) fed by a user-mode host. It ports the capture path of Microsoft's
//! `simpleaudiosample` (Copyright (c) Microsoft Corporation, MS-PL); PLAN.md lists where it
//! deliberately departs from it.
//!
//! Milestone M3: the endpoint "Microphone (hostmic)", whose capture stream the driver fills
//! itself with a 1 kHz test tone; M4 hands the buffer and the clock to a user-mode host.
//!
//! - [`com`]: COM-style objects and interface references.
//! - [`ks`]: Kernel Streaming descriptor and property helpers.
//! - [`adapter`]: the PnP device and its subdevices.
//! - [`topology`]: the topology miniport.
//! - [`wave`]: the WaveRT miniport.
//! - [`stream`]: the capture stream and its timer.

#![cfg_attr(not(test), no_std)]

extern crate alloc;
#[cfg(not(test))]
extern crate wdk_panic;

pub mod adapter;
pub mod com;
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
use wdk_sys::{DRIVER_OBJECT, IRP_MJ_PNP, NT_SUCCESS, NTSTATUS, PCUNICODE_STRING};

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

    // PnP requests go through the adapter first, then to PortCls.
    driver.MajorFunction[IRP_MJ_PNP as usize] = Some(adapter::dispatch_pnp);
    // The driver's unload routine is installed whatever PortCls left there, so that the driver
    // stays unloadable; it chains to PortCls's when there is one (Microsoft's sample does the
    // same).
    if let Some(unload) = driver.DriverUnload {
        PORTCLS_UNLOAD.store(unload as *mut c_void, Ordering::Release);
    }
    driver.DriverUnload = Some(driver_unload);
    status
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
