//! The topology miniport: the filter that tells Windows what kind of endpoint the capture path
//! ends in (a microphone) and describes its jack.
//!
//! Ported from the `MicArray1` topology of Microsoft's `simpleaudiosample`, simplified to a
//! plain microphone (PLAN.md, M3): two bridge pins joined directly, no volume or mute node, no
//! microphone-array geometry. The jack follows the host (M4): connected while a host is attached,
//! with presence detection, and a jack change event for the audio endpoint builder, so that
//! Windows shows the microphone unplugged without a host.
//!
//! ```text
//!               +-----------+
//!   microphone  |           |  bridge to the
//!   (pin 0) --->|  topology |---> wave filter (pin 1)
//!               +-----------+
//! ```

use core::{
    ffi::c_void,
    ptr,
    sync::atomic::{AtomicPtr, AtomicU32, Ordering},
};

use hostmic_sys::{
    EPcxConnectionType, EPcxGenLocation, EPcxGeoLocation, EPxcPortConnection,
    IMiniportTopologyVtbl, IPortEvents, JACKDESC2_PRESENCE_DETECT_CAPABILITY, KSDATAFORMAT,
    KSDATAFORMAT__bindgen_ty_1, KSDATARANGE, KSEVENT_PINCAPS_CHANGENOTIFICATIONS,
    KSEVENT_TYPE_BASICSUPPORT, KSEVENT_TYPE_ENABLE, KSJACK_DESCRIPTION, KSJACK_DESCRIPTION2,
    KSPIN_COMMUNICATION, KSPIN_DATAFLOW, KSPIN_DESCRIPTOR, KSPIN_DESCRIPTOR__bindgen_ty_1,
    KSPROPERTY_JACK, KSPROPERTY_TYPE_BASICSUPPORT, KSPROPERTY_TYPE_GET, PCAUTOMATION_TABLE,
    PCCONNECTION_DESCRIPTOR, PCEVENT_ITEM, PCEVENT_VERB_ADD, PCFILTER_DESCRIPTOR, PCMETHOD_ITEM,
    PCNODE_DESCRIPTOR, PCPIN_DESCRIPTOR, PCPROPERTY_ITEM, PKSDATARANGE, PPCEVENT_REQUEST,
    PPCFILTER_DESCRIPTOR, PPCPROPERTY_REQUEST, PPORTTOPOLOGY, PRESOURCELIST, PUNKNOWN, guid,
};
use wdk_sys::{
    NTSTATUS, PULONG, PVOID, STATUS_INVALID_DEVICE_REQUEST, STATUS_INVALID_PARAMETER,
    STATUS_NOT_IMPLEMENTED, STATUS_SUCCESS, STATUS_UNSUCCESSFUL, ULONG,
};

use wdk::println;

use crate::com::{self, ComObject, Unknown};
use crate::host_link::LinkRef;
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
    /// The adapter's host link: whether a host is attached, and where jack events go.
    link: LinkRef,
    /// The port's `IPortEvents`: one reference, owned by the miniport, for the jack event's
    /// enables; null until `Init`.
    events: AtomicPtr<IPortEvents>,
}

impl Drop for TopologyMiniport {
    /// Runs at `PASSIVE_LEVEL`, when PortCls releases the miniport (the filters stop or the
    /// device goes away): the link lets go of this port's events, and so does the miniport.
    fn drop(&mut self) {
        let events = self.events.swap(ptr::null_mut(), Ordering::AcqRel);
        self.link.get().clear_jack_events(events);
        // SAFETY: the reference `init` stored, owned by this miniport (or null).
        drop(unsafe { Unknown::from_raw(events.cast()) });
    }
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
pub fn new(link: LinkRef) -> Option<Unknown> {
    com::into_unknown(TopologyMiniport {
        vtable: &VTABLE,
        references: AtomicU32::new(1),
        link,
        events: AtomicPtr::new(ptr::null_mut()),
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

/// `IMiniportTopology::Init`: hands the port's `IPortEvents` to the host link, which reports
/// jack changes through it.
unsafe extern "C" fn init(
    this: *mut c_void,
    _unknown_adapter: PUNKNOWN,
    _resource_list: PRESOURCELIST,
    port: PPORTTOPOLOGY,
) -> NTSTATUS {
    // Two references to the port's `IPortEvents`: the miniport's own, for the jack event's
    // enables, and the link's, for jack changes.
    // SAFETY: PortCls passes its live port for the call.
    let own = unsafe { com::query_borrowed(port.cast(), &guid::IID_IPortEvents) };
    // SAFETY: as above.
    let shared = unsafe { com::query_borrowed(port.cast(), &guid::IID_IPortEvents) };
    let (Some(own), Some(shared)) = (own, shared) else {
        println!("hostmic: the topology port has no IPortEvents: no jack events");
        return STATUS_SUCCESS;
    };
    // SAFETY: called through the miniport's vtable, at offset 0.
    let miniport = unsafe { com::object::<TopologyMiniport, 0>(this) };
    let old = miniport
        .events
        .swap(own.into_raw().cast(), Ordering::AcqRel);
    // SAFETY: a reference a previous `Init` stored, owned by this miniport (or null).
    drop(unsafe { Unknown::from_raw(old.cast()) });
    miniport.link.get().set_jack_events(shared);
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
    // SAFETY: for a filter property, `MajorTarget` is the miniport's `IUnknown`, at offset 0.
    let miniport = unsafe { com::object::<TopologyMiniport, 0>(request.MajorTarget.cast()) };
    let connected = miniport.link.get().is_attached();
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
                // Connected while a host is attached.
                IsConnected: i32::from(connected),
            },
        ),
        JACK_DESCRIPTION2 => ks::reply_multiple_item(
            request,
            KSJACK_DESCRIPTION2 {
                DeviceStateInfo: 0,
                // The driver knows whether the jack is connected (the host's presence) and
                // reports changes: Windows shows the microphone unplugged without a host.
                JackCapabilities: JACKDESC2_PRESENCE_DETECT_CAPABILITY,
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

/// `KSEVENT_PINCAPS_JACKINFOCHANGE`: the audio endpoint builder enables it on the microphone
/// pin to learn when the jack changes (sysvad's jack events).
static FILTER_EVENTS: Shared<[PCEVENT_ITEM; 1]> = Shared([PCEVENT_ITEM {
    Set: &guid::KSEVENTSETID_PinCapsChange,
    Id: KSEVENT_PINCAPS_CHANGENOTIFICATIONS::KSEVENT_PINCAPS_JACKINFOCHANGE.cast_unsigned(),
    Flags: KSEVENT_TYPE_ENABLE | KSEVENT_TYPE_BASICSUPPORT,
    Handler: Some(jack_event),
}]);

/// Enables and disables of the jack change event: an added entry goes to the port's list,
/// which `HostLink` signals on attach and detach; PortCls removes entries itself.
unsafe extern "C" fn jack_event(request: PPCEVENT_REQUEST) -> NTSTATUS {
    // SAFETY: PortCls passes a valid request for the duration of the call.
    let Some(request) = (unsafe { request.as_ref() }) else {
        return STATUS_INVALID_PARAMETER;
    };
    if request.Verb != PCEVENT_VERB_ADD {
        return STATUS_SUCCESS;
    }
    // SAFETY: for a filter event, `MajorTarget` is the miniport's `IUnknown`, at offset 0;
    // PortCls holds the miniport for the request.
    let miniport = unsafe { com::object::<TopologyMiniport, 0>(request.MajorTarget.cast()) };
    let events = miniport.events.load(Ordering::Acquire);
    if events.is_null() {
        // Nothing added: PortCls must not believe the entry is in a list.
        return STATUS_UNSUCCESSFUL;
    }
    // SAFETY: the miniport's own reference to its port's `IPortEvents`, alive with the
    // miniport.
    let vtable = unsafe { (*events).lpVtbl };
    // SAFETY: an interface's vtable is valid for its lifetime.
    let Some(add) = (unsafe { &*vtable }).AddEventToEventList else {
        return STATUS_UNSUCCESSFUL;
    };
    // SAFETY: a live interface and the entry of this `PCEVENT_VERB_ADD` request: it goes to
    // the list of this miniport's own port.
    unsafe { add(events.cast(), request.EventEntry) };
    STATUS_SUCCESS
}

static FILTER_AUTOMATION: Shared<PCAUTOMATION_TABLE> = Shared(PCAUTOMATION_TABLE {
    PropertyItemSize: size_ulong::<PCPROPERTY_ITEM>(),
    PropertyCount: 2,
    Properties: FILTER_PROPERTIES.0.as_ptr(),
    MethodItemSize: size_ulong::<PCMETHOD_ITEM>(),
    MethodCount: 0,
    Methods: ptr::null(),
    EventItemSize: size_ulong::<PCEVENT_ITEM>(),
    EventCount: 1,
    Events: FILTER_EVENTS.0.as_ptr(),
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
