//! The WaveRT miniport: the filter whose capture pin the audio engine streams from.
//!
//! Ported from `CMiniportWaveRT` and the `MicArray` wave tables of Microsoft's
//! `simpleaudiosample`, reduced to one format (48 kHz, 16-bit, stereo) and the default signal
//! processing mode (PLAN.md, M3).
//!
//! ```text
//!               +---------------+
//!   bridge  --->| ADC           |---> capture host pin (pin 1)
//!   (pin 0)     | (node 0)      |
//!               +---------------+
//! ```

use core::{ffi::c_void, ptr, slice, sync::atomic::AtomicU32};

use hostmic_driver_core::format::{
    self as audio_format, BITS_PER_SAMPLE, BYTES_PER_SECOND, CHANNEL_MASK_STEREO, CHANNELS,
    FRAME_BYTES, SAMPLE_RATE,
};
use hostmic_sys::{
    IMiniportWaveRTVtbl, KSDATAFORMAT, KSDATAFORMAT__bindgen_ty_1,
    KSDATAFORMAT_WAVEFORMATEXTENSIBLE, KSDATARANGE, KSDATARANGE_AUDIO, KSPIN_COMMUNICATION,
    KSPIN_DATAFLOW, KSPIN_DESCRIPTOR, KSPIN_DESCRIPTOR__bindgen_ty_1, KSPROPERTY_PIN,
    KSPROPERTY_TYPE_BASICSUPPORT, KSPROPERTY_TYPE_SET, PCAUTOMATION_TABLE, PCCONNECTION_DESCRIPTOR,
    PCEVENT_ITEM, PCFILTER_DESCRIPTOR, PCMETHOD_ITEM, PCNODE_DESCRIPTOR, PCPIN_DESCRIPTOR,
    PCPROPERTY_ITEM, PKSDATAFORMAT, PKSDATARANGE, PMINIPORTWAVERTSTREAM, PPCFILTER_DESCRIPTOR,
    PPCPROPERTY_REQUEST, PPORTWAVERT, PPORTWAVERTSTREAM, PRESOURCELIST, PUNKNOWN,
    WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE__bindgen_ty_1, guid, tWAVEFORMATEX,
};
use wdk_sys::{
    BOOLEAN, NTSTATUS, PDEVICE_DESCRIPTION, PULONG, PVOID, STATUS_BUFFER_OVERFLOW,
    STATUS_BUFFER_TOO_SMALL, STATUS_INSUFFICIENT_RESOURCES, STATUS_INVALID_DEVICE_REQUEST,
    STATUS_INVALID_PARAMETER, STATUS_NO_MATCH, STATUS_NOT_IMPLEMENTED, STATUS_SUCCESS, ULONG,
};

use crate::com::{self, ComObject, Unknown};
use crate::host_link::LinkRef;
use crate::ks::{self, PCFILTER_NODE, Shared, size_ulong};
use crate::stream;

/// Pin 0: the bridge from the topology filter (`PcRegisterPhysicalConnection`).
pub const PIN_BRIDGE: ULONG = 0;
/// Pin 1: the capture pin the audio engine streams from.
pub const PIN_HOST: ULONG = 1;
/// Node 0: the analog-to-digital converter.
const NODE_ADC: ULONG = 0;

/// The WaveRT miniport object.
#[repr(C)]
pub struct WaveMiniport {
    /// `IUnknown`, `IMiniport` and `IMiniportWaveRT`, which extend one another.
    vtable: &'static IMiniportWaveRTVtbl,
    references: AtomicU32,
    /// The adapter's host link, handed to each stream.
    link: LinkRef,
}

// SAFETY: `#[repr(C)]`, the vtable at offset 0 is `VTABLE`, whose `IUnknown` entries are the
// generic ones for this type and offset 0; the only state is atomic.
unsafe impl ComObject for WaveMiniport {
    const INTERFACES: &'static [(wdk_sys::GUID, usize)] =
        &[(guid::IID_IMiniport, 0), (guid::IID_IMiniportWaveRT, 0)];

    fn references(&self) -> &AtomicU32 {
        &self.references
    }
}

/// Creates a WaveRT miniport, or `None` when the pool is exhausted.
#[must_use]
pub fn new(link: LinkRef) -> Option<Unknown> {
    com::into_unknown(WaveMiniport {
        vtable: &VTABLE,
        references: AtomicU32::new(1),
        link,
    })
}

static VTABLE: IMiniportWaveRTVtbl = IMiniportWaveRTVtbl {
    QueryInterface: Some(com::query_interface::<WaveMiniport, 0>),
    AddRef: Some(com::add_ref::<WaveMiniport, 0>),
    Release: Some(com::release::<WaveMiniport, 0>),
    GetDescription: Some(get_description),
    DataRangeIntersection: Some(data_range_intersection),
    Init: Some(init),
    NewStream: Some(new_stream),
    GetDeviceDescription: Some(get_device_description),
};

/// The one format the capture pin streams.
static FORMAT: Shared<KSDATAFORMAT_WAVEFORMATEXTENSIBLE> =
    Shared(KSDATAFORMAT_WAVEFORMATEXTENSIBLE {
        DataFormat: KSDATAFORMAT {
            __bindgen_anon_1: KSDATAFORMAT__bindgen_ty_1 {
                FormatSize: size_ulong::<KSDATAFORMAT_WAVEFORMATEXTENSIBLE>(),
                Flags: 0,
                SampleSize: 0,
                Reserved: 0,
                MajorFormat: guid::KSDATAFORMAT_TYPE_AUDIO,
                SubFormat: guid::KSDATAFORMAT_SUBTYPE_PCM,
                Specifier: guid::KSDATAFORMAT_SPECIFIER_WAVEFORMATEX,
            },
        },
        WaveFormatExt: WAVEFORMATEXTENSIBLE {
            Format: tWAVEFORMATEX {
                wFormatTag: 0xFFFE, // WAVE_FORMAT_EXTENSIBLE
                nChannels: CHANNELS,
                nSamplesPerSec: SAMPLE_RATE,
                nAvgBytesPerSec: BYTES_PER_SECOND,
                #[allow(clippy::cast_possible_truncation, reason = "4 bytes")]
                nBlockAlign: FRAME_BYTES as u16,
                wBitsPerSample: BITS_PER_SAMPLE,
                #[allow(clippy::cast_possible_truncation, reason = "22 bytes")]
                cbSize: audio_format::EXTENSIBLE_EXTRA_BYTES as u16,
            },
            Samples: WAVEFORMATEXTENSIBLE__bindgen_ty_1 {
                wValidBitsPerSample: BITS_PER_SAMPLE,
            },
            dwChannelMask: CHANNEL_MASK_STEREO,
            SubFormat: guid::KSDATAFORMAT_SUBTYPE_PCM,
        },
    });

// The format the driver hands out is one it accepts.
const _: () = assert!(
    size_of::<KSDATAFORMAT_WAVEFORMATEXTENSIBLE>()
        == audio_format::KSDATAFORMAT_BYTES
            + audio_format::WAVEFORMATEX_BYTES
            + audio_format::EXTENSIBLE_EXTRA_BYTES
);

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

/// `IMiniport::DataRangeIntersection`: on the capture pin, the one format, when the client's
/// range allows it; PortCls's default elsewhere.
unsafe extern "C" fn data_range_intersection(
    _this: *mut c_void,
    pin: ULONG,
    client_range: PKSDATARANGE,
    my_range: PKSDATARANGE,
    output_length: ULONG,
    resultant_format: PVOID,
    resultant_length: PULONG,
) -> NTSTATUS {
    // SAFETY: forwards the caller's arguments.
    let status = unsafe {
        intersect(
            pin,
            client_range,
            my_range,
            output_length,
            resultant_format,
            resultant_length,
        )
    };
    wdk::println!(
        "hostmic: DataRangeIntersection pin {pin}, output {output_length}: {status:#010x}"
    );
    status
}

/// The intersection itself.
///
/// # Safety
///
/// PortCls's arguments for `DataRangeIntersection`.
unsafe fn intersect(
    pin: ULONG,
    client_range: PKSDATARANGE,
    _my_range: PKSDATARANGE,
    output_length: ULONG,
    resultant_format: PVOID,
    resultant_length: PULONG,
) -> NTSTATUS {
    if pin != PIN_HOST {
        return STATUS_NOT_IMPLEMENTED;
    }
    if client_range.is_null() || resultant_length.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: PortCls passes a valid client data range, at least a KSDATARANGE.
    let range = unsafe { client_range.read_unaligned() };
    // SAFETY: the struct member is the union's only meaningful view.
    let header = unsafe { range.__bindgen_anon_1 };
    if !com::guid_eq(
        &header.Specifier,
        &guid::KSDATAFORMAT_SPECIFIER_WAVEFORMATEX,
    ) {
        return STATUS_NOT_IMPLEMENTED;
    }
    if usize::try_from(header.FormatSize).unwrap_or(0) >= size_of::<KSDATARANGE_AUDIO>() {
        // SAFETY: the range says it is at least a KSDATARANGE_AUDIO.
        let audio = unsafe { client_range.cast::<KSDATARANGE_AUDIO>().read_unaligned() };
        let fits = audio.MaximumChannels >= ULONG::from(CHANNELS)
            && (audio.MinimumBitsPerSample..=audio.MaximumBitsPerSample)
                .contains(&ULONG::from(BITS_PER_SAMPLE))
            && (audio.MinimumSampleFrequency..=audio.MaximumSampleFrequency).contains(&SAMPLE_RATE);
        if !fits {
            return STATUS_NO_MATCH;
        }
    }
    let needed = size_ulong::<KSDATAFORMAT_WAVEFORMATEXTENSIBLE>();
    if output_length == 0 {
        // SAFETY: `resultant_length` is writable (checked not null above).
        unsafe { resultant_length.write(needed) };
        return STATUS_BUFFER_OVERFLOW;
    }
    if output_length < needed || resultant_format.is_null() {
        return STATUS_BUFFER_TOO_SMALL;
    }
    // SAFETY: the output buffer holds `output_length >= needed` bytes.
    unsafe {
        resultant_format
            .cast::<KSDATAFORMAT_WAVEFORMATEXTENSIBLE>()
            .write_unaligned(FORMAT.0);
    };
    // SAFETY: as above.
    unsafe { resultant_length.write(needed) };
    STATUS_SUCCESS
}

/// `IMiniportWaveRT::Init`: nothing to set up.
unsafe extern "C" fn init(
    _this: *mut c_void,
    _unknown_adapter: PUNKNOWN,
    _resource_list: PRESOURCELIST,
    _port: PPORTWAVERT,
) -> NTSTATUS {
    STATUS_SUCCESS
}

/// `IMiniportWaveRT::NewStream`: a capture stream on the host pin, in the one format.
unsafe extern "C" fn new_stream(
    this: *mut c_void,
    out_stream: *mut PMINIPORTWAVERTSTREAM,
    port_stream: PPORTWAVERTSTREAM,
    pin: ULONG,
    capture: BOOLEAN,
    data_format: PKSDATAFORMAT,
) -> NTSTATUS {
    if out_stream.is_null() || port_stream.is_null() || data_format.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    wdk::println!("hostmic: NewStream pin {pin}, capture {capture}");
    // SAFETY: `out_stream` is writable (checked not null above).
    unsafe { out_stream.write(ptr::null_mut()) };
    if pin != PIN_HOST || capture == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: PortCls passes a valid data format of `FormatSize` bytes.
    let size = unsafe { data_format.cast::<ULONG>().read_unaligned() };
    let Ok(size) = usize::try_from(size) else {
        return STATUS_INVALID_PARAMETER;
    };
    // SAFETY: as above: the format's own size is readable.
    let bytes = unsafe { slice::from_raw_parts(data_format.cast::<u8>(), size) };
    if !audio_format::is_supported(bytes) {
        return STATUS_NO_MATCH;
    }
    // SAFETY: called through the miniport's vtable, at offset 0.
    let link = unsafe { com::object::<WaveMiniport, 0>(this) }.link;
    // SAFETY: `port_stream` is the port's stream for this new stream, valid for its life.
    let Some(stream) = (unsafe { stream::new(port_stream, link) }) else {
        return STATUS_INSUFFICIENT_RESOURCES;
    };
    // The stream's reference passes to PortCls.
    // SAFETY: `out_stream` is writable; the stream's offset-0 interface is an
    // `IMiniportWaveRTStream`.
    unsafe { out_stream.write(stream.into_raw().cast()) };
    STATUS_SUCCESS
}

/// `IMiniportWaveRT::GetDeviceDescription`: the sample's description (no real DMA).
unsafe extern "C" fn get_device_description(
    _this: *mut c_void,
    description: PDEVICE_DESCRIPTION,
) -> NTSTATUS {
    if description.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let value = wdk_sys::DEVICE_DESCRIPTION {
        Master: 1,
        ScatterGather: 1,
        Dma32BitAddresses: 1,
        InterfaceType: wdk_sys::_INTERFACE_TYPE::PCIBus,
        MaximumLength: ULONG::MAX,
        ..Default::default()
    };
    // SAFETY: `description` is writable (checked not null above).
    unsafe { description.write(value) };
    STATUS_SUCCESS
}

/// Handles `KSPROPERTY_PIN_PROPOSEDATAFORMAT` on the capture pin: whether a format would be
/// accepted.
unsafe extern "C" fn propose_format(request: PPCPROPERTY_REQUEST) -> NTSTATUS {
    // SAFETY: forwards the caller's request.
    let status = unsafe { propose(request) };
    wdk::println!("hostmic: PROPOSEDATAFORMAT: {status:#010x}");
    status
}

/// The `PROPOSEDATAFORMAT` handler itself.
///
/// # Safety
///
/// PortCls's request.
unsafe fn propose(request: PPCPROPERTY_REQUEST) -> NTSTATUS {
    // SAFETY: PortCls passes a valid request for the duration of the call.
    let Some(request) = (unsafe { request.as_mut() }) else {
        return STATUS_INVALID_PARAMETER;
    };
    if ks::instance_pin(request) != Some(PIN_HOST) {
        return STATUS_INVALID_PARAMETER;
    }
    if request.Verb & KSPROPERTY_TYPE_BASICSUPPORT != 0 {
        return ks::basic_support(request, KSPROPERTY_TYPE_BASICSUPPORT | KSPROPERTY_TYPE_SET);
    }
    if request.ValueSize == 0 {
        request.ValueSize = size_ulong::<KSDATAFORMAT_WAVEFORMATEXTENSIBLE>();
        return STATUS_BUFFER_OVERFLOW;
    }
    if request.ValueSize < size_ulong::<KSDATAFORMAT>() || request.Value.is_null() {
        return STATUS_BUFFER_TOO_SMALL;
    }
    if request.Verb & KSPROPERTY_TYPE_SET == 0 {
        return STATUS_INVALID_DEVICE_REQUEST;
    }
    let Ok(size) = usize::try_from(request.ValueSize) else {
        return STATUS_INVALID_PARAMETER;
    };
    // SAFETY: PortCls's value buffer holds `ValueSize` bytes.
    let bytes = unsafe { slice::from_raw_parts(request.Value.cast::<u8>(), size) };
    if audio_format::is_proposal_supported(bytes) {
        STATUS_SUCCESS
    } else {
        STATUS_NO_MATCH
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

static PCM_RANGE: Shared<KSDATARANGE_AUDIO> = Shared(KSDATARANGE_AUDIO {
    DataRange: KSDATAFORMAT {
        __bindgen_anon_1: KSDATAFORMAT__bindgen_ty_1 {
            FormatSize: size_ulong::<KSDATARANGE_AUDIO>(),
            Flags: 0,
            SampleSize: 0,
            Reserved: 0,
            MajorFormat: guid::KSDATAFORMAT_TYPE_AUDIO,
            SubFormat: guid::KSDATAFORMAT_SUBTYPE_PCM,
            Specifier: guid::KSDATAFORMAT_SPECIFIER_WAVEFORMATEX,
        },
    },
    MaximumChannels: CHANNELS as ULONG,
    MinimumBitsPerSample: BITS_PER_SAMPLE as ULONG,
    MaximumBitsPerSample: BITS_PER_SAMPLE as ULONG,
    MinimumSampleFrequency: SAMPLE_RATE,
    MaximumSampleFrequency: SAMPLE_RATE,
});

static PCM_RANGES: Shared<[PKSDATARANGE; 1]> =
    Shared([(&raw const PCM_RANGE.0).cast_mut().cast::<KSDATARANGE>()]);

static PIN_PROPERTIES: Shared<[PCPROPERTY_ITEM; 1]> = Shared([PCPROPERTY_ITEM {
    Set: &guid::KSPROPSETID_Pin,
    Id: KSPROPERTY_PIN::KSPROPERTY_PIN_PROPOSEDATAFORMAT.cast_unsigned(),
    Flags: KSPROPERTY_TYPE_SET | KSPROPERTY_TYPE_BASICSUPPORT,
    Handler: Some(propose_format),
}]);

static FILTER_AUTOMATION: Shared<PCAUTOMATION_TABLE> = Shared(PCAUTOMATION_TABLE {
    PropertyItemSize: size_ulong::<PCPROPERTY_ITEM>(),
    PropertyCount: 1,
    Properties: PIN_PROPERTIES.0.as_ptr(),
    MethodItemSize: size_ulong::<PCMETHOD_ITEM>(),
    MethodCount: 0,
    Methods: ptr::null(),
    EventItemSize: size_ulong::<PCEVENT_ITEM>(),
    EventCount: 0,
    Events: ptr::null(),
    Reserved: 0,
});

static PINS: Shared<[PCPIN_DESCRIPTOR; 2]> = Shared([
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
            DataFlow: KSPIN_DATAFLOW::KSPIN_DATAFLOW_IN,
            Communication: KSPIN_COMMUNICATION::KSPIN_COMMUNICATION_NONE,
            Category: &guid::KSCATEGORY_AUDIO,
            Name: ptr::null(),
            __bindgen_anon_1: KSPIN_DESCRIPTOR__bindgen_ty_1 { Reserved: 0 },
        },
    },
    PCPIN_DESCRIPTOR {
        // One capture stream at a time.
        MaxGlobalInstanceCount: 1,
        MaxFilterInstanceCount: 1,
        MinFilterInstanceCount: 0,
        AutomationTable: ptr::null(),
        KsPinDescriptor: KSPIN_DESCRIPTOR {
            InterfacesCount: 0,
            Interfaces: ptr::null(),
            MediumsCount: 0,
            Mediums: ptr::null(),
            DataRangesCount: 1,
            DataRanges: PCM_RANGES.0.as_ptr(),
            DataFlow: KSPIN_DATAFLOW::KSPIN_DATAFLOW_OUT,
            Communication: KSPIN_COMMUNICATION::KSPIN_COMMUNICATION_SINK,
            Category: &guid::KSCATEGORY_AUDIO,
            Name: &guid::KSAUDFNAME_RECORDING_CONTROL,
            __bindgen_anon_1: KSPIN_DESCRIPTOR__bindgen_ty_1 { Reserved: 0 },
        },
    },
]);

static NODES: Shared<[PCNODE_DESCRIPTOR; 1]> = Shared([PCNODE_DESCRIPTOR {
    Flags: 0,
    AutomationTable: ptr::null(),
    Type: &guid::KSNODETYPE_ADC,
    Name: ptr::null(),
}]);

static CONNECTIONS: [PCCONNECTION_DESCRIPTOR; 2] = [
    PCCONNECTION_DESCRIPTOR {
        FromNode: PCFILTER_NODE,
        FromNodePin: PIN_BRIDGE,
        ToNode: NODE_ADC,
        ToNodePin: 1,
    },
    PCCONNECTION_DESCRIPTOR {
        FromNode: NODE_ADC,
        FromNodePin: 0,
        ToNode: PCFILTER_NODE,
        ToNodePin: PIN_HOST,
    },
];

static FILTER: Shared<PCFILTER_DESCRIPTOR> = Shared(PCFILTER_DESCRIPTOR {
    Version: 0,
    AutomationTable: &raw const FILTER_AUTOMATION.0,
    PinSize: size_ulong::<PCPIN_DESCRIPTOR>(),
    PinCount: 2,
    Pins: PINS.0.as_ptr(),
    NodeSize: size_ulong::<PCNODE_DESCRIPTOR>(),
    NodeCount: 1,
    Nodes: NODES.0.as_ptr(),
    ConnectionCount: 2,
    Connections: CONNECTIONS.as_ptr(),
    // The WaveRT port supplies the default categories itself (audio, capture, real time), as
    // the INF's AddInterface lines declare.
    CategoryCount: 0,
    Categories: ptr::null(),
});
