//! # hostmic driver
//!
//! Milestone M1: a minimal KMDF driver that only proves the toolchain (build, packaging, test
//! signing, loading in a test VM). It creates one device and logs its lifecycle with
//! `DbgPrint`; it does no audio yet.
//!
//! Adapted from the `sample-kmdf-driver` example of
//! [windows-drivers-rs](https://github.com/microsoft/windows-drivers-rs)
//! (Copyright (c) Microsoft Corporation, MIT OR Apache-2.0).

#![cfg_attr(not(test), no_std)]

#[cfg(not(test))]
extern crate wdk_panic;

use wdk::println;
#[cfg(not(test))]
use wdk_alloc::WdkAllocator;
use wdk_sys::{
    DRIVER_OBJECT, NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT, ULONG, WDF_DRIVER_CONFIG,
    WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES, WDFDEVICE, WDFDEVICE_INIT, WDFDRIVER,
    call_unsafe_wdf_function_binding,
};

#[cfg(not(test))]
#[global_allocator]
static GLOBAL_ALLOCATOR: WdkAllocator = WdkAllocator;

/// Size of `WDF_DRIVER_CONFIG`, checked at compile time to fit the `ULONG` WDF expects.
#[allow(clippy::cast_possible_truncation)]
const WDF_DRIVER_CONFIG_SIZE: ULONG = {
    let size = core::mem::size_of::<WDF_DRIVER_CONFIG>();
    assert!(size <= ULONG::MAX as usize);
    size as ULONG
};

/// The driver's entry point, called once by the kernel when the driver is loaded.
///
/// # Safety
///
/// Called by the kernel only, with a valid driver object and registry path.
// SAFETY: `DriverEntry` is the symbol name the kernel and WDF look for; no other function in
// this crate exports it.
#[cfg_attr(not(test), unsafe(export_name = "DriverEntry"))]
pub unsafe extern "system" fn driver_entry(
    driver: &mut DRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    println!("hostmic: DriverEntry");
    driver.DriverUnload = Some(driver_unload);

    let mut config = WDF_DRIVER_CONFIG {
        Size: WDF_DRIVER_CONFIG_SIZE,
        EvtDriverDeviceAdd: Some(evt_driver_device_add),
        ..WDF_DRIVER_CONFIG::default()
    };

    let status;
    // SAFETY: `driver` and `registry_path` come from the kernel and are valid for this call;
    // no object attributes and no output handle are allowed by `WdfDriverCreate`; `config` is
    // a valid, fully initialised `WDF_DRIVER_CONFIG` that outlives the call.
    unsafe {
        status = call_unsafe_wdf_function_binding!(
            WdfDriverCreate,
            core::ptr::from_mut(driver) as PDRIVER_OBJECT,
            registry_path,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut config,
            WDF_NO_HANDLE.cast::<WDFDRIVER>(),
        );
    }
    println!("hostmic: WdfDriverCreate returned {status:#010x}");
    status
}

/// Called by WDF when the device described by `hostmic_driver.inf` is added.
extern "C" fn evt_driver_device_add(
    _driver: WDFDRIVER,
    mut device_init: *mut WDFDEVICE_INIT,
) -> NTSTATUS {
    let mut device: WDFDEVICE = WDF_NO_HANDLE.cast();
    let status;
    // SAFETY: `device_init` is provided by WDF for this call and is valid; no object
    // attributes are allowed; `device` receives the created handle.
    unsafe {
        status = call_unsafe_wdf_function_binding!(
            WdfDeviceCreate,
            &raw mut device_init,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut device,
        );
    }
    println!("hostmic: device added, WdfDeviceCreate returned {status:#010x}");
    status
}

/// Called by the kernel when the driver is unloaded.
extern "C" fn driver_unload(_driver: *mut DRIVER_OBJECT) {
    println!("hostmic: driver unloaded");
}

#[cfg(test)]
mod tests {
    /// Ensures test targets compile without linking the WDK libraries.
    #[test]
    fn unload_logs_without_crashing() {
        super::driver_unload(core::ptr::null_mut());
    }
}
