//! The adapter: the PnP device that PortCls drives, and the subdevices (filters) it registers.
//!
//! Ported from `adapter.cpp` and `CAdapterCommon` of Microsoft's `simpleaudiosample`, without
//! the adapter COM object and without adapter power management (PLAN.md, M3): the adapter's
//! state is a plain struct in the driver's own part of the device extension, after the part
//! PortCls reserves for itself.

use core::{
    ffi::c_void,
    ptr::{self, NonNull},
};

use hostmic_sys::{
    IPort, PPORT, PRESOURCELIST, PcAddAdapterDevice, PcDispatchIrp, PcGetPhysicalDeviceObject,
    PcNewPort, PcRegisterPhysicalConnection, PcRegisterSubdevice, guid,
};
use wdk::println;
use wdk_sys::{
    _DEVICE_RELATION_TYPE, DEVICE_OBJECT, DRIVER_OBJECT, GUID, IRP, IRP_MN_QUERY_DEVICE_RELATIONS,
    IRP_MN_REMOVE_DEVICE, IRP_MN_STOP_DEVICE, IRP_MN_SURPRISE_REMOVAL, NT_SUCCESS, NTSTATUS,
    PDEVICE_OBJECT, PIRP, STATUS_INSUFFICIENT_RESOURCES, STATUS_SUCCESS, STATUS_UNSUCCESSFUL,
    ULONG, ntddk::IoInvalidateDeviceRelations,
};

use crate::com::Unknown;
use crate::host_link::{HostLink, LinkRef};
use crate::ks::size_ulong;
use crate::{host_device, topology, wave};

/// Bytes at the start of the device extension that belong to PortCls: `portcls.h`'s
/// `PORT_CLASS_DEVICE_EXTENSION_SIZE`, `64 * sizeof(ULONG_PTR)`.
const PORT_CLASS_DEVICE_EXTENSION_SIZE: usize = 64 * size_of::<usize>();

/// Offset of the [`HostLink`] in the device extension, after PortCls's part and [`Adapter`]. Kept
/// out of [`Adapter`], which PnP requests borrow mutably, because the stream and the host
/// interface share it.
const LINK_OFFSET: usize = PORT_CLASS_DEVICE_EXTENSION_SIZE + size_of::<Adapter>();
const _: () = assert!(LINK_OFFSET.is_multiple_of(align_of::<HostLink>()));

/// Device extension size: PortCls's part, [`Adapter`], then [`HostLink`].
const DEVICE_EXTENSION_SIZE: ULONG = size_ulong::<[u8; LINK_OFFSET]>() + size_ulong::<HostLink>();

/// Subdevices of the adapter: the topology filter and the wave filter.
const MAX_SUBDEVICES: ULONG = 2;

/// Reference string of the topology filter's device interfaces. The INF's `AddInterface`
/// lines use the same name.
static TOPOLOGY_NAME: [u16; 12] = wide("TopologyMic");
/// Reference string of the wave filter's device interfaces, likewise.
static WAVE_NAME: [u16; 8] = wide("WaveMic");

/// The adapter's state, in the device extension after PortCls's part.
///
/// All of it is accessed from PnP requests (start, stop, removal), which the PnP manager
/// serializes for a device.
#[repr(C)]
struct Adapter {
    /// The ports of the registered filters; dropping them releases the driver's references.
    topology_port: Option<Unknown>,
    wave_port: Option<Unknown>,
    /// The host interface's PDO, created on the first start and reported in the bus
    /// relations until the adapter is removed ([`host_device`]).
    host: Option<NonNull<DEVICE_OBJECT>>,
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

/// The adapter's [`HostLink`].
///
/// # Safety
///
/// `device` is the FDO created by `PcAddAdapterDevice` in [`add_device`]. The link lives as
/// long as the device object's extension.
pub(crate) unsafe fn link(device: PDEVICE_OBJECT) -> LinkRef {
    // SAFETY: `device` is a valid device object (caller's contract).
    let extension = unsafe { (*device).DeviceExtension };
    // The extension holds a `HostLink` at `LINK_OFFSET`, zeroed by `IoCreateDevice` (its
    // initial state) and only ever accessed through shared references.
    // Never null: `PcAddAdapterDevice` allocated the extension (`DEVICE_EXTENSION_SIZE` bytes).
    let link = NonNull::new(extension.wrapping_byte_add(LINK_OFFSET).cast::<HostLink>())
        .unwrap_or(NonNull::dangling());
    // SAFETY: the adapter's link, in the extension, which outlives the stream and the host
    // interface's device (`host_link` module documentation).
    unsafe { LinkRef::new(link) }
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

/// `StartDevice`, called by PortCls on `IRP_MN_START_DEVICE`: creates the host interface's PDO
/// (on the first start), then creates and registers the filters.
///
/// The host interface comes first, and the start fails with its status when it cannot be
/// created: without it no host can feed the microphone, which would otherwise be present, and
/// silent, with nothing to say why. A failed start shows in Device Manager (code 10) instead.
/// A PDO created by a start that fails later was never reported: the adapter's removal deletes
/// it ([`host_device::detach`]).
unsafe extern "C" fn start_device(
    device: PDEVICE_OBJECT,
    irp: PIRP,
    resources: PRESOURCELIST,
) -> NTSTATUS {
    // SAFETY: `device` is the adapter's FDO.
    let link = unsafe { link(device) };
    if let Err(status) = link.get().init() {
        println!("hostmic: StartDevice returned {status:#010x}: no shared buffer");
        return status;
    }
    // SAFETY: `device` is the adapter's FDO, in a PnP request.
    let adapter = unsafe { adapter(device) };
    let created = adapter.host.is_none();
    if created {
        // SAFETY: `device` is the adapter's FDO, at PASSIVE_LEVEL.
        match unsafe { host_device::create(device, link) } {
            Ok(host) => adapter.host = Some(host),
            Err(status) => {
                println!(
                    "hostmic: StartDevice returned {status:#010x}: host interface not created"
                );
                return status;
            }
        }
    }
    // SAFETY: `device`, `irp` and `resources` come from PortCls for this start request.
    let status = match unsafe { install_filters(device, irp, resources, link) } {
        Ok((topology_port, wave_port)) => {
            adapter.topology_port = Some(topology_port);
            adapter.wave_port = Some(wave_port);
            if created {
                // Once started: the PnP manager queries the bus relations, which report the
                // new child.
                // SAFETY: `device` is the adapter's FDO, at PASSIVE_LEVEL.
                unsafe { enumerate_children(device) };
            }
            STATUS_SUCCESS
        }
        Err(status) => status,
    };
    println!("hostmic: StartDevice returned {status:#010x}");
    status
}

/// Installs the topology filter and the wave filter, then connects the topology's bridge pin
/// to the wave's (Microsoft's `InstallEndpointFilters` and `ConnectTopologies`). Returns both
/// ports' `IUnknown`.
///
/// # Safety
///
/// `device`, `irp` and `resources` come from PortCls for a start request of the adapter.
unsafe fn install_filters(
    device: PDEVICE_OBJECT,
    irp: PIRP,
    resources: PRESOURCELIST,
    link: LinkRef,
) -> Result<(Unknown, Unknown), NTSTATUS> {
    let topology_miniport = topology::new(link).ok_or(STATUS_INSUFFICIENT_RESOURCES)?;
    // SAFETY: the caller's contract.
    let topology_port = unsafe {
        install_subdevice(
            device,
            irp,
            resources,
            &TOPOLOGY_NAME,
            &guid::CLSID_PortTopology,
            &topology_miniport,
        )
    }?;
    let wave_miniport = wave::new(link).ok_or(STATUS_INSUFFICIENT_RESOURCES)?;
    // SAFETY: the caller's contract.
    let wave_port = unsafe {
        install_subdevice(
            device,
            irp,
            resources,
            &WAVE_NAME,
            &guid::CLSID_PortWaveRT,
            &wave_miniport,
        )
    }?;
    // SAFETY: `device` is the adapter's FDO; both ports are registered subdevices of it.
    let status = unsafe {
        PcRegisterPhysicalConnection(
            device,
            topology_port.as_ptr(),
            topology::PIN_BRIDGE,
            wave_port.as_ptr(),
            wave::PIN_BRIDGE,
        )
    };
    if !NT_SUCCESS(status) {
        return Err(status);
    }
    Ok((topology_port, wave_port))
}

/// Creates a port of class `port_class`, binds `miniport` to it and registers the pair as a
/// subdevice named `name`. Returns the port's `IUnknown`; the port holds its own reference to the miniport.
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
    // The port's canonical `IUnknown`, which `PcRegisterPhysicalConnection` expects (the
    // sample keeps the same).
    port.query(&guid::IID_IUnknown).ok_or(STATUS_UNSUCCESSFUL)
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

/// Asks the PnP manager to query the adapter's bus relations, which now include the host
/// interface's PDO.
///
/// # Safety
///
/// `device` is the adapter's FDO; `PASSIVE_LEVEL`.
unsafe fn enumerate_children(device: PDEVICE_OBJECT) {
    let mut physical: PDEVICE_OBJECT = ptr::null_mut();
    // SAFETY: `device` is a PortCls FDO; `physical` is a writable out-parameter.
    let status = unsafe { PcGetPhysicalDeviceObject(device, &raw mut physical) };
    if !NT_SUCCESS(status) || physical.is_null() {
        println!("hostmic: no physical device object: {status:#010x}");
        return;
    }
    // SAFETY: the adapter's PDO, at PASSIVE_LEVEL.
    unsafe { IoInvalidateDeviceRelations(physical, _DEVICE_RELATION_TYPE::BusRelations) };
}

/// `IRP_MJ_PNP` dispatch for the adapter's FDO: releases the driver's references to the ports
/// when the device stops or goes away, reports the host interface's PDO as its child, then
/// lets PortCls handle the request (Microsoft's `PnpHandler`).
///
/// # Safety
///
/// Called by the driver's dispatch routine with the adapter's FDO and a PnP IRP.
pub unsafe fn dispatch_pnp(device: *mut DEVICE_OBJECT, irp: *mut IRP) -> NTSTATUS {
    // SAFETY: `irp` is a valid IRP being dispatched to this driver.
    let stack = unsafe { current_stack_location(irp) };
    // SAFETY: an IRP being dispatched has a valid current stack location for the call.
    let stack = unsafe { &*stack };
    let minor = stack.MinorFunction;
    // SAFETY: `device` is the adapter's FDO (the driver's dispatch routine sends the host
    // interface's requests elsewhere), in a PnP request.
    let adapter = unsafe { adapter(device) };
    match u32::from(minor) {
        IRP_MN_REMOVE_DEVICE | IRP_MN_SURPRISE_REMOVAL | IRP_MN_STOP_DEVICE => {
            let topology = adapter.topology_port.take();
            let wave = adapter.wave_port.take();
            if topology.is_some() || wave.is_some() {
                println!("hostmic: device stopping (minor function {minor:#04x}), ports released");
            }
            if u32::from(minor) == IRP_MN_REMOVE_DEVICE
                && let Some(host) = adapter.host.take()
            {
                // SAFETY: the live host interface PDO, in the adapter's removal.
                unsafe { host_device::detach(host) };
            }
            if u32::from(minor) == IRP_MN_REMOVE_DEVICE {
                // SAFETY: `device` is the adapter's FDO, still alive in its removal.
                unsafe { link(device) }.get().shutdown();
            }
        }
        IRP_MN_QUERY_DEVICE_RELATIONS => {
            // SAFETY: the parameters of a relations query.
            let relation = unsafe { stack.Parameters.QueryDeviceRelations.Type };
            if relation == _DEVICE_RELATION_TYPE::BusRelations
                && let Some(host) = adapter.host
            {
                // SAFETY: the adapter's bus relations query, with its live child.
                if let Err(status) = unsafe { host_device::report(irp, host) } {
                    // `report` failed the request, which still goes down (a bus driver does
                    // not complete it): the PnP manager keeps the relations it knows.
                    println!("hostmic: host interface not reported: {status:#010x}");
                }
            }
        }
        _ => {}
    }
    // SAFETY: forwards the request PortCls installed itself for.
    unsafe { PcDispatchIrp(device, irp) }
}

/// `IoGetCurrentIrpStackLocation`, an inline function of `wdm.h` that `wdk-sys` does not bind.
///
/// # Safety
///
/// `irp` is a valid IRP owned by the caller's driver for the duration of the call.
pub(crate) unsafe fn current_stack_location(irp: *mut IRP) -> *mut wdk_sys::IO_STACK_LOCATION {
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
pub(crate) const fn wide<const N: usize>(s: &str) -> [u16; N] {
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
