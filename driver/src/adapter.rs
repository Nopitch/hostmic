//! The adapter: the PnP device that PortCls drives, and the subdevices (filters) it registers.
//!
//! Ported from `adapter.cpp` and `CAdapterCommon` of Microsoft's `simpleaudiosample`, without
//! the adapter COM object and without adapter power management (PLAN.md, M3): the adapter's
//! state is a plain struct in the driver's own part of the device extension, after the part
//! PortCls reserves for itself.

use core::{ffi::c_void, ptr};

use hostmic_sys::{
    IPort, PPORT, PRESOURCELIST, PcAddAdapterDevice, PcDispatchIrp, PcNewPort, PcRegisterSubdevice,
    guid,
};
use wdk::println;
use wdk_sys::{
    DEVICE_OBJECT, DRIVER_OBJECT, GUID, IRP, IRP_MN_REMOVE_DEVICE, IRP_MN_STOP_DEVICE,
    IRP_MN_SURPRISE_REMOVAL, NT_SUCCESS, NTSTATUS, PDEVICE_OBJECT, PIRP,
    STATUS_INSUFFICIENT_RESOURCES, STATUS_SUCCESS, STATUS_UNSUCCESSFUL, ULONG,
};

use crate::com::Unknown;
use crate::ks::size_ulong;
use crate::topology;

/// Bytes at the start of the device extension that belong to PortCls: `portcls.h`'s
/// `PORT_CLASS_DEVICE_EXTENSION_SIZE`, `64 * sizeof(ULONG_PTR)`.
const PORT_CLASS_DEVICE_EXTENSION_SIZE: usize = 64 * size_of::<usize>();

/// Device extension size: PortCls's part, then [`Adapter`].
const DEVICE_EXTENSION_SIZE: ULONG =
    size_ulong::<[u8; PORT_CLASS_DEVICE_EXTENSION_SIZE]>() + size_ulong::<Adapter>();

/// Subdevices of the adapter: the topology filter now, the wave filter later.
const MAX_SUBDEVICES: ULONG = 2;

/// Reference string of the topology filter's device interfaces. The INF's `AddInterface`
/// lines use the same name.
static TOPOLOGY_NAME: [u16; 12] = wide("TopologyMic");

/// The adapter's state, in the device extension after PortCls's part.
///
/// All of it is accessed from PnP requests (start, stop, removal), which the PnP manager
/// serializes for a device.
#[repr(C)]
struct Adapter {
    /// The topology port, once registered; dropping it releases the driver's reference.
    topology_port: Option<Unknown>,
}

/// The adapter of a device created by [`add_device`].
///
/// # Safety
///
/// `device` is the FDO created by `PcAddAdapterDevice` in [`add_device`]; the caller runs in
/// a PnP request for it (which the PnP manager serializes), so the reference is unique.
unsafe fn adapter<'a>(device: PDEVICE_OBJECT) -> &'a mut Adapter {
    // SAFETY: `device` is a valid device object (caller's contract).
    let extension = unsafe { (*device).DeviceExtension };
    let adapter = extension
        .wrapping_byte_add(PORT_CLASS_DEVICE_EXTENSION_SIZE)
        .cast::<Adapter>();
    // SAFETY: the extension was allocated with `DEVICE_EXTENSION_SIZE` bytes, aligned for
    // pointers, and zeroed by `IoCreateDevice`; all zeroes is a valid `Adapter` (`None`).
    // Uniqueness: the caller's contract.
    unsafe { &mut *adapter }
}

/// `AddDevice`: PnP hands the driver a new device; PortCls creates its FDO.
///
/// # Safety
///
/// Called by the PnP manager with valid objects.
pub unsafe extern "C" fn add_device(
    driver: *mut DRIVER_OBJECT,
    physical_device: *mut DEVICE_OBJECT,
) -> NTSTATUS {
    // SAFETY: valid objects from the PnP manager; `start_device` matches `PCPFNSTARTDEVICE`.
    let status = unsafe {
        PcAddAdapterDevice(
            driver,
            physical_device,
            Some(start_device),
            MAX_SUBDEVICES,
            DEVICE_EXTENSION_SIZE,
        )
    };
    println!("hostmic: AddDevice returned {status:#010x}");
    status
}

/// `StartDevice`, called by PortCls on `IRP_MN_START_DEVICE`: creates and registers the
/// filters.
unsafe extern "C" fn start_device(
    device: PDEVICE_OBJECT,
    irp: PIRP,
    resources: PRESOURCELIST,
) -> NTSTATUS {
    let status = match topology::new() {
        // SAFETY: `device`, `irp` and `resources` come from PortCls for this start request.
        Some(miniport) => match unsafe {
            install_subdevice(
                device,
                irp,
                resources,
                &TOPOLOGY_NAME,
                &guid::CLSID_PortTopology,
                &miniport,
            )
        } {
            Ok(port) => {
                // SAFETY: `device` is the adapter's FDO, in a PnP request.
                unsafe { adapter(device) }.topology_port = Some(port);
                STATUS_SUCCESS
            }
            Err(status) => status,
        },
        None => STATUS_INSUFFICIENT_RESOURCES,
    };
    println!("hostmic: StartDevice returned {status:#010x}");
    status
}

/// Creates a port of class `port_class`, binds `miniport` to it and registers the pair as a
/// subdevice named `name`. Returns the port, which holds its own reference to the miniport.
///
/// Microsoft's `CAdapterCommon::InstallSubdevice`, without the interface properties.
///
/// # Safety
///
/// `device`, `irp` and `resources` come from PortCls for a start request of the adapter.
unsafe fn install_subdevice(
    device: PDEVICE_OBJECT,
    irp: PIRP,
    resources: PRESOURCELIST,
    name: &'static [u16],
    port_class: &GUID,
    miniport: &Unknown,
) -> Result<Unknown, NTSTATUS> {
    let mut raw: PPORT = ptr::null_mut();
    // SAFETY: `raw` is a writable out-parameter, `port_class` a valid GUID.
    let status = unsafe { PcNewPort(&raw mut raw, port_class) };
    if !NT_SUCCESS(status) {
        return Err(status);
    }
    // SAFETY: `PcNewPort` succeeded: `raw` is a port carrying one reference, now owned here.
    let port = unsafe { Unknown::from_raw(raw.cast()) }.ok_or(STATUS_UNSUCCESSFUL)?;

    // SAFETY: `port` is a live `IPort`; the other arguments are valid for this start request.
    // No adapter object: `UnknownAdapter` is optional.
    let status = unsafe {
        port_init(
            port.as_ptr().cast(),
            device,
            irp,
            miniport.as_ptr(),
            ptr::null_mut(),
            resources,
        )
    };
    if !NT_SUCCESS(status) {
        return Err(status);
    }

    // SAFETY: `name` is a NUL-terminated wide string that lives forever and that PortCls only
    // reads (the `PWSTR` parameter is not const in the C prototype only).
    let status =
        unsafe { PcRegisterSubdevice(device, name.as_ptr().cast_mut(), port.as_ptr().cast()) };
    if !NT_SUCCESS(status) {
        return Err(status);
    }
    Ok(port)
}

/// `IPort::Init`.
///
/// # Safety
///
/// `port` is a live `IPort`; the other arguments are valid for this start request.
unsafe fn port_init(
    port: *mut IPort,
    device: PDEVICE_OBJECT,
    irp: PIRP,
    miniport: *mut hostmic_sys::IUnknown,
    adapter: *mut hostmic_sys::IUnknown,
    resources: PRESOURCELIST,
) -> NTSTATUS {
    // SAFETY: `port` is a live interface (caller's contract).
    let vtable = unsafe { (*port).lpVtbl };
    // SAFETY: an interface's vtable is valid for its lifetime.
    let Some(init) = (unsafe { (*vtable).Init }) else {
        return STATUS_UNSUCCESSFUL;
    };
    // SAFETY: the caller's contract.
    unsafe {
        init(
            port.cast::<c_void>(),
            device,
            irp,
            miniport,
            adapter,
            resources,
        )
    }
}

/// `IRP_MJ_PNP` dispatch: releases the driver's references to the ports when the device stops
/// or goes away, then lets PortCls handle the request (Microsoft's `PnpHandler`).
///
/// # Safety
///
/// Called by the I/O manager with a valid device object of this driver and a PnP IRP.
pub unsafe extern "C" fn dispatch_pnp(device: *mut DEVICE_OBJECT, irp: *mut IRP) -> NTSTATUS {
    // SAFETY: `irp` is a valid IRP being dispatched to this driver.
    let stack = unsafe { current_stack_location(irp) };
    // SAFETY: an IRP being dispatched has a valid current stack location.
    let minor = unsafe { (*stack).MinorFunction };
    if matches!(
        u32::from(minor),
        IRP_MN_REMOVE_DEVICE | IRP_MN_SURPRISE_REMOVAL | IRP_MN_STOP_DEVICE
    ) {
        // SAFETY: `device` is the adapter's FDO (this driver creates no other device), in a
        // PnP request.
        let adapter = unsafe { adapter(device) };
        if adapter.topology_port.take().is_some() {
            println!("hostmic: device stopping (minor function {minor:#04x}), ports released");
        }
    }
    // SAFETY: forwards the request PortCls installed itself for.
    unsafe { PcDispatchIrp(device, irp) }
}

/// `IoGetCurrentIrpStackLocation`, an inline function of `wdm.h` that `wdk-sys` does not bind.
///
/// # Safety
///
/// `irp` is a valid IRP owned by the caller's driver for the duration of the call.
unsafe fn current_stack_location(irp: *mut IRP) -> *mut wdk_sys::IO_STACK_LOCATION {
    // SAFETY: `irp` is valid (caller's contract); only the field's address is taken.
    let overlay = unsafe { &raw const (*irp).Tail.Overlay };
    // SAFETY: `overlay` points into the valid IRP; only the field's address is taken.
    let location = unsafe {
        &raw const (*overlay)
            .__bindgen_anon_2
            .__bindgen_anon_1
            .CurrentStackLocation
    };
    // SAFETY: for an IRP being dispatched, `Tail.Overlay` is the active member of its union and
    // `CurrentStackLocation` the active member of its own (wdm.h's
    // `IoGetCurrentIrpStackLocation` reads the same field).
    unsafe { location.read() }
}

/// A NUL-terminated UTF-16 copy of the ASCII string `s`, at compile time.
const fn wide<const N: usize>(s: &str) -> [u16; N] {
    let bytes = s.as_bytes();
    assert!(
        bytes.len() + 1 == N,
        "N is the length plus the terminating NUL"
    );
    let mut out = [0; N];
    let mut i = 0;
    while i < bytes.len() {
        assert!(bytes[i].is_ascii());
        out[i] = bytes[i] as u16;
        i += 1;
    }
    out
}
