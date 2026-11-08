//! The topology miniport: the filter that tells Windows what kind of endpoint the capture path
//! ends in (a microphone) and describes its jack.
//!
//! Ported from the `MicArray1` topology of Microsoft's `simpleaudiosample`, simplified to a
//! plain microphone (PLAN.md, M3): two bridge pins joined directly, no volume or mute node, no
//! microphone-array geometry. The jack is always connected until the host interface (M4) makes
//! it follow the host.
//!
//! ```text
//!               +-----------+
//!   microphone  |           |  bridge to the
//!   (pin 0) --->|  topology |---> wave filter (pin 1)
//!               +-----------+
//! ```

use core::{ffi::c_void, ptr, sync::atomic::AtomicU32};

use hostmic_sys::{
    EPcxConnectionType, EPcxGenLocation, EPcxGeoLocation, EPxcPortConnection,
    IMiniportTopologyVtbl, KSDATAFORMAT, KSDATAFORMAT__bindgen_ty_1, KSDATARANGE,
    KSJACK_DESCRIPTION, KSJACK_DESCRIPTION2, KSPIN_COMMUNICATION, KSPIN_DATAFLOW, KSPIN_DESCRIPTOR,
    KSPIN_DESCRIPTOR__bindgen_ty_1, KSPROPERTY_JACK, KSPROPERTY_TYPE_BASICSUPPORT,
    KSPROPERTY_TYPE_GET, PCAUTOMATION_TABLE, PCCONNECTION_DESCRIPTOR, PCEVENT_ITEM,
    PCFILTER_DESCRIPTOR, PCMETHOD_ITEM, PCNODE_DESCRIPTOR, PCPIN_DESCRIPTOR, PCPROPERTY_ITEM,
    PKSDATARANGE, PPCFILTER_DESCRIPTOR, PPCPROPERTY_REQUEST, PPORTTOPOLOGY, PRESOURCELIST,
    PUNKNOWN, guid,
};
use wdk_sys::{
    NTSTATUS, PULONG, PVOID, STATUS_INVALID_DEVICE_REQUEST, STATUS_INVALID_PARAMETER,
    STATUS_NOT_IMPLEMENTED, STATUS_SUCCESS, TRUE, ULONG,
};

use crate::com::{self, ComObject, Unknown};
use crate::ks::{self, PCFILTER_NODE, Shared, size_ulong};

/// Pin 0: the microphone itself, the endpoint's physical side.
pub const PIN_MICROPHONE: ULONG = 0;
/// Pin 1: the bridge to the wave filter's bridge pin (`PcRegisterPhysicalConnection`).
pub const PIN_BRIDGE: ULONG = 1;

/// The topology miniport object.
#[repr(C)]
pub struct TopologyMiniport {
    /// `IUnknown`, `IMiniport` and `IMiniportTopology`, which extend one another.
    vtable: &'static IMiniportTopologyVtbl,
    references: AtomicU32,
}

// SAFETY: `#[repr(C)]`, the vtable at offset 0 is `VTABLE`, whose `IUnknown` entries are the
// generic ones for this type and offset 0; the only state is atomic.
unsafe impl ComObject for TopologyMiniport {
    const INTERFACES: &'static [(wdk_sys::GUID, usize)] =
        &[(guid::IID_IMiniport, 0), (guid::IID_IMiniportTopology, 0)];

    fn references(&self) -> &AtomicU32 {
        &self.references
    }
}

/// Creates a topology miniport, or `None` when the pool is exhausted.
#[must_use]
pub fn new() -> Option<Unknown> {
    com::into_unknown(TopologyMiniport {
        vtable: &VTABLE,
        references: AtomicU32::new(1),
    })
}

static VTABLE: IMiniportTopologyVtbl = IMiniportTopologyVtbl {
    QueryInterface: Some(com::query_interface::<TopologyMiniport, 0>),
    AddRef: Some(com::add_ref::<TopologyMiniport, 0>),
    Release: Some(com::release::<TopologyMiniport, 0>),
    GetDescription: Some(get_description),
    DataRangeIntersection: Some(data_range_intersection),
    Init: Some(init),
};

/// `IMiniport::GetDescription`: the static filter descriptor.
unsafe extern "C" fn get_description(
    _this: *mut c_void,
    description: *mut PPCFILTER_DESCRIPTOR,
) -> NTSTATUS {
    if description.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // The descriptor is immutable; PortCls only reads it through this pointer.
    let filter = (&raw const FILTER.0).cast_mut();
    // SAFETY: `description` is a writable out-parameter (checked not null above).
    unsafe { description.write(filter) };
    STATUS_SUCCESS
}

/// `IMiniport::DataRangeIntersection`: not implemented, so PortCls applies its default
/// intersection to the bridge pins' analog ranges.
unsafe extern "C" fn data_range_intersection(
    _this: *mut c_void,
    _pin: ULONG,
    _data_range: PKSDATARANGE,
    _matching_data_range: PKSDATARANGE,
    _output_buffer_length: ULONG,
    _resultant_format: PVOID,
    _resultant_format_length: PULONG,
) -> NTSTATUS {
    STATUS_NOT_IMPLEMENTED
}

/// `IMiniportTopology::Init`: nothing to set up. The port is not kept: M4 will keep it to
/// signal jack changes.
unsafe extern "C" fn init(
    _this: *mut c_void,
    _unknown_adapter: PUNKNOWN,
    _resource_list: PRESOURCELIST,
    _port: PPORTTOPOLOGY,
) -> NTSTATUS {
    STATUS_SUCCESS
}

/// Handles `KSPROPERTY_JACK_DESCRIPTION` and `KSPROPERTY_JACK_DESCRIPTION2` on the microphone
/// pin.
unsafe extern "C" fn jack_property(request: PPCPROPERTY_REQUEST) -> NTSTATUS {
    // SAFETY: PortCls passes a valid request for the duration of the call.
    let Some(request) = (unsafe { request.as_mut() }) else {
        return STATUS_INVALID_PARAMETER;
    };
    if ks::instance_pin(request) != Some(PIN_MICROPHONE) {
        return STATUS_INVALID_DEVICE_REQUEST;
    }
    if request.Verb & KSPROPERTY_TYPE_BASICSUPPORT != 0 {
        return ks::basic_support(request, KSPROPERTY_TYPE_BASICSUPPORT | KSPROPERTY_TYPE_GET);
    }
    if request.Verb & KSPROPERTY_TYPE_GET == 0 {
        return STATUS_INVALID_DEVICE_REQUEST;
    }
    // SAFETY: PortCls points `PropertyItem` at the item of `FILTER_PROPERTIES` it matched.
    let Some(item) = (unsafe { request.PropertyItem.as_ref() }) else {
        return STATUS_INVALID_PARAMETER;
    };
    match item.Id {
        JACK_DESCRIPTION => ks::reply_multiple_item(
            request,
            KSJACK_DESCRIPTION {
                ChannelMapping: 0,
                Color: 0,
                ConnectionType: EPcxConnectionType::eConnTypeUnknown,
                GeoLocation: EPcxGeoLocation::eGeoLocFront,
                GenLocation: EPcxGenLocation::eGenLocPrimaryBox,
                PortConnection: EPxcPortConnection::ePortConnIntegratedDevice,
                // Always connected in M3; M4 reports the host's presence here.
                IsConnected: TRUE.cast_signed(),
            },
        ),
        JACK_DESCRIPTION2 => ks::reply_multiple_item(
            request,
            KSJACK_DESCRIPTION2 {
                DeviceStateInfo: 0,
                // No presence detection yet (M4: JACKDESC2_PRESENCE_DETECT_CAPABILITY).
                JackCapabilities: 0,
            },
        ),
        _ => STATUS_INVALID_DEVICE_REQUEST,
    }
}

static ANALOG_RANGE: Shared<KSDATARANGE> = Shared(KSDATAFORMAT {
    __bindgen_anon_1: KSDATAFORMAT__bindgen_ty_1 {
        FormatSize: size_ulong::<KSDATARANGE>(),
        Flags: 0,
        SampleSize: 0,
        Reserved: 0,
        MajorFormat: guid::KSDATAFORMAT_TYPE_AUDIO,
        SubFormat: guid::KSDATAFORMAT_SUBTYPE_ANALOG,
        Specifier: guid::KSDATAFORMAT_SPECIFIER_NONE,
    },
});

static ANALOG_RANGES: Shared<[PKSDATARANGE; 1]> = Shared([(&raw const ANALOG_RANGE.0).cast_mut()]);

/// A bridge pin: no instances, analog audio, flowing `data_flow`.
const fn bridge_pin(
    data_flow: KSPIN_DATAFLOW::Type,
    category: &'static wdk_sys::GUID,
) -> PCPIN_DESCRIPTOR {
    PCPIN_DESCRIPTOR {
        MaxGlobalInstanceCount: 0,
        MaxFilterInstanceCount: 0,
        MinFilterInstanceCount: 0,
        AutomationTable: ptr::null(),
        KsPinDescriptor: KSPIN_DESCRIPTOR {
            InterfacesCount: 0,
            Interfaces: ptr::null(),
            MediumsCount: 0,
            Mediums: ptr::null(),
            DataRangesCount: 1,
            DataRanges: ANALOG_RANGES.0.as_ptr(),
            DataFlow: data_flow,
            Communication: KSPIN_COMMUNICATION::KSPIN_COMMUNICATION_NONE,
            Category: category,
            // No custom name: Windows names the endpoint after the category, "Microphone".
            Name: ptr::null(),
            __bindgen_anon_1: KSPIN_DESCRIPTOR__bindgen_ty_1 { Reserved: 0 },
        },
    }
}

static PINS: Shared<[PCPIN_DESCRIPTOR; 2]> = Shared([
    bridge_pin(
        KSPIN_DATAFLOW::KSPIN_DATAFLOW_IN,
        &guid::KSNODETYPE_MICROPHONE,
    ),
    bridge_pin(KSPIN_DATAFLOW::KSPIN_DATAFLOW_OUT, &guid::KSCATEGORY_AUDIO),
]);

static CONNECTIONS: [PCCONNECTION_DESCRIPTOR; 1] = [PCCONNECTION_DESCRIPTOR {
    FromNode: PCFILTER_NODE,
    FromNodePin: PIN_MICROPHONE,
    ToNode: PCFILTER_NODE,
    ToNodePin: PIN_BRIDGE,
}];

const fn jack_item(id: ULONG) -> PCPROPERTY_ITEM {
    PCPROPERTY_ITEM {
        Set: &guid::KSPROPSETID_Jack,
        Id: id,
        Flags: KSPROPERTY_TYPE_GET | KSPROPERTY_TYPE_BASICSUPPORT,
        Handler: Some(jack_property),
    }
}

static FILTER_PROPERTIES: Shared<[PCPROPERTY_ITEM; 2]> =
    Shared([jack_item(JACK_DESCRIPTION), jack_item(JACK_DESCRIPTION2)]);

static FILTER_AUTOMATION: Shared<PCAUTOMATION_TABLE> = Shared(PCAUTOMATION_TABLE {
    PropertyItemSize: size_ulong::<PCPROPERTY_ITEM>(),
    PropertyCount: 2,
    Properties: FILTER_PROPERTIES.0.as_ptr(),
    MethodItemSize: size_ulong::<PCMETHOD_ITEM>(),
    MethodCount: 0,
    Methods: ptr::null(),
    EventItemSize: size_ulong::<PCEVENT_ITEM>(),
    EventCount: 0,
    Events: ptr::null(),
    Reserved: 0,
});

static FILTER: Shared<PCFILTER_DESCRIPTOR> = Shared(PCFILTER_DESCRIPTOR {
    Version: 0,
    AutomationTable: &raw const FILTER_AUTOMATION.0,
    PinSize: size_ulong::<PCPIN_DESCRIPTOR>(),
    PinCount: 2,
    Pins: PINS.0.as_ptr(),
    // Ignored without nodes, but set as in every Microsoft table.
    NodeSize: size_ulong::<PCNODE_DESCRIPTOR>(),
    NodeCount: 0,
    Nodes: ptr::null(),
    ConnectionCount: 1,
    Connections: CONNECTIONS.as_ptr(),
    // The topology port supplies the default categories itself: audio and topology, as the
    // INF's AddInterface lines declare.
    CategoryCount: 0,
    Categories: ptr::null(),
});

/// `KSPROPERTY_JACK_DESCRIPTION`, as the `ULONG` of `PCPROPERTY_ITEM::Id` (bindgen types the
/// C enum as `i32`).
const JACK_DESCRIPTION: ULONG = KSPROPERTY_JACK::KSPROPERTY_JACK_DESCRIPTION.cast_unsigned();
/// `KSPROPERTY_JACK_DESCRIPTION2`, as a `ULONG`.
const JACK_DESCRIPTION2: ULONG = KSPROPERTY_JACK::KSPROPERTY_JACK_DESCRIPTION2.cast_unsigned();
