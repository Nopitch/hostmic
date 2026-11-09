//! Generates the PortCls / Kernel Streaming bindings from the WDK headers.
//!
//! Three outputs, all in `OUT_DIR`:
//! - `bindings.rs`: bindgen over `portcls.h` and `ksmedia.h` in C mode, restricted to the
//!   allowlists below. Everything the WDM headers already define comes from `wdk-sys`, so that
//!   `DEVICE_OBJECT`, `MDL`, `IRP`… are the same types on both sides.
//! - `guids.rs`: the GUIDs as Rust constants. bindgen only sees GUIDs as `extern const GUID`
//!   declarations, and the PortCls IIDs/CLSIDs are defined in no WDK library, so their values
//!   are read from the headers through MSVC's preprocessor.
//! - `layout.c` (compiled with MSVC into a static library) and `layout_tests.rs`: the test
//!   suite compares Rust sizes, alignments and field offsets with what MSVC computes.
//!
//! It also compiles `c/map_user.c`, the driver's one C routine (a structured exception handler,
//! which Rust cannot write), into the static library `hostmic_sys_c`.

use std::{
    env,
    error::Error,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use bindgen::callbacks::{DeriveTrait, ImplementsTrait, ParseCallbacks};
use wdk_build::{ApiSubset, BuilderExt, Config, configure_wdk_library_build_and_then};

/// The types left to `wdk-sys` (`GUID`, `ULONG`, `PVOID`…) are `Copy` there. Without this,
/// bindgen cannot derive `Copy` for anything containing them, and C unions such as
/// `KSDATAFORMAT` fall back to an over-aligned emulation that packed structs cannot contain.
#[derive(Debug)]
struct WdkSysTypesAreCopy;

impl ParseCallbacks for WdkSysTypesAreCopy {
    fn blocklisted_type_implements_trait(
        &self,
        _name: &str,
        derive_trait: DeriveTrait,
    ) -> Option<ImplementsTrait> {
        matches!(derive_trait, DeriveTrait::Copy).then_some(ImplementsTrait::Yes)
    }
}

/// Headers bound, after the base WDM headers. In C mode, `portcls.h` never defines the
/// `INTERFACE` macro, so every method's `This` parameter would silently become a pointer to
/// WDM's unrelated `INTERFACE` struct. So `portcls.h`'s own includes come first, in its order
/// (`ks.h` needs the real `INTERFACE` struct, `punknown.h` `#undef`s the macro), then `This` is
/// made a `void *` for `portcls.h` alone.
const HEADERS: &str = "\
#include <wdmsec.h>
#include <windef.h>
#define NOBITMAP
#include <mmreg.h>
#undef NOBITMAP
#include <ks.h>
#include <ksmedia.h>
#include <punknown.h>
#undef INTERFACE
#define INTERFACE void
#include <portcls.h>
";

/// PortCls functions used by the capture path of Microsoft's `simpleaudiosample`, and
/// `wdmsec.h`'s secure device creation for the host interface (PLAN.md, M4).
const FUNCTIONS: &[&str] = &[
    "PcAddAdapterDevice",
    "PcDispatchIrp",
    "PcGetPhysicalDeviceObject",
    "PcInitializeAdapterDriver",
    "PcNewPort",
    "PcRegisterAdapterPowerManagement",
    "PcRegisterPhysicalConnection",
    "PcRegisterSubdevice",
    // wdmsec.h (`IoCreateDeviceSecure` is a macro naming it), in `wdmsec.lib`.
    "WdmlibIoCreateDeviceSecure",
];

/// Types, matched with an optional `_` (struct tag) and `P` (pointer typedef) prefix.
const TYPES: &[&str] = &[
    // portcls.h: descriptors and automation tables.
    "PCAUTOMATION_TABLE",
    "PCCONNECTION_DESCRIPTOR",
    "PCEVENT_ITEM",
    "PCEVENT_REQUEST",
    "PCFILTER_DESCRIPTOR",
    "PCMETHOD_ITEM",
    "PCMETHOD_REQUEST",
    "PCNODE_DESCRIPTOR",
    "PCPFNEVENT_HANDLER",
    "PCPFNMETHOD_HANDLER",
    "PCPFNPROPERTY_HANDLER",
    "PCPFNSTARTDEVICE",
    "PCPIN_DESCRIPTOR",
    "PCPROPERTY_ITEM",
    "PCPROPERTY_REQUEST",
    // portcls.h / punknown.h: COM interfaces (struct, vtable, pointer typedef).
    "IUnknown",
    "IUnknownVtbl",
    "UNKNOWN",
    "IAdapterPowerManagement",
    "IAdapterPowerManagementVtbl",
    "ADAPTERPOWERMANAGEMENT",
    "IMiniport",
    "IMiniportVtbl",
    "MINIPORT",
    "IMiniportAudioSignalProcessing",
    "IMiniportAudioSignalProcessingVtbl",
    "MINIPORTAUDIOSIGNALPROCESSING",
    "IMiniportTopology",
    "IMiniportTopologyVtbl",
    "MINIPORTTOPOLOGY",
    "IMiniportWaveRT",
    "IMiniportWaveRTVtbl",
    "MINIPORTWAVERT",
    "IMiniportWaveRTInputStream",
    "IMiniportWaveRTInputStreamVtbl",
    "MINIPORTWAVERTINPUTSTREAM",
    "IMiniportWaveRTStream",
    "IMiniportWaveRTStreamVtbl",
    "MINIPORTWAVERTSTREAM",
    "IMiniportWaveRTStreamNotification",
    "IMiniportWaveRTStreamNotificationVtbl",
    "MINIPORTWAVERTSTREAMNOTIFICATION",
    "IPort",
    "IPortVtbl",
    "PORT",
    "IPortEvents",
    "IPortEventsVtbl",
    "PORTEVENTS",
    "IPortTopology",
    "IPortTopologyVtbl",
    "PORTTOPOLOGY",
    "IPortWaveRT",
    "IPortWaveRTVtbl",
    "PORTWAVERT",
    "IPortWaveRTStream",
    "IPortWaveRTStreamVtbl",
    "PORTWAVERTSTREAM",
    "IRegistryKey",
    "IRegistryKeyVtbl",
    "REGISTRYKEY",
    "IResourceList",
    "IResourceListVtbl",
    "RESOURCELIST",
    "IUnregisterPhysicalConnection",
    "IUnregisterPhysicalConnectionVtbl",
    "UNREGISTERPHYSICALCONNECTION",
    "IUnregisterSubdevice",
    "IUnregisterSubdeviceVtbl",
    "UNREGISTERSUBDEVICE",
    // ks.h
    "KSATTRIBUTE",
    "KSATTRIBUTE_LIST",
    "KSDATAFORMAT",
    "KSDATARANGE",
    "KSEVENT",
    "KSEVENT_ENTRY",
    "KSEVENT_PINCAPS_CHANGENOTIFICATIONS",
    "KSIDENTIFIER",
    "KSMETHOD",
    "KSMULTIPLE_ITEM",
    "KSP_PIN",
    "KSPIN_COMMUNICATION",
    "KSPIN_DATAFLOW",
    "KSPIN_DESCRIPTOR",
    "KSPIN_INTERFACE",
    "KSPIN_MEDIUM",
    "KSPROPERTY",
    "KSPROPERTY_BOUNDS_LONG",
    "KSPROPERTY_DESCRIPTION",
    "KSPROPERTY_GENERAL",
    "KSPROPERTY_MEMBERSHEADER",
    "KSPROPERTY_PIN",
    "KSPROPERTY_STEPPING_LONG",
    "KSSTATE",
    "KSTOPOLOGY_CONNECTION",
    // ksmedia.h
    "EPcxConnectionType",
    "EPcxGenLocation",
    "EPcxGeoLocation",
    "EPxcPortConnection",
    "KSATTRIBUTE_AUDIOSIGNALPROCESSING_MODE",
    "KSAUDIO_MIC_ARRAY_GEOMETRY",
    "KSAUDIO_MICROPHONE_COORDINATES",
    "KSAUDIO_POSITION",
    "KSAUDIO_PRESENTATION_POSITION",
    "KSDATAFORMAT_WAVEFORMATEX",
    "KSDATAFORMAT_WAVEFORMATEXTENSIBLE",
    "KSDATARANGE_AUDIO",
    "KSJACK_DESCRIPTION",
    "KSJACK_DESCRIPTION2",
    "KSMICARRAY_MICARRAYTYPE",
    "KSMICARRAY_MICTYPE",
    "KSPROPERTY_AUDIO",
    "KSPROPERTY_JACK",
    "KSRTAUDIO_HWLATENCY",
    "KSRTAUDIO_HWREGISTER",
    // mmreg.h
    "WAVEFORMATEX",
    "WAVEFORMATEXTENSIBLE",
    // windef.h / minwindef.h: user-mode names that wdk-sys does not have.
    "BOOL",
    "WORD",
];

/// Integer constants (`#define`s); enums come with their types above.
const VARS: &[&str] = &[
    "KSAUDIO_SPEAKER_MONO",
    "KSAUDIO_SPEAKER_STEREO",
    "JACKDESC2_PRESENCE_DETECT_CAPABILITY",
    "KSDATAFORMAT_ATTRIBUTES",
    "KSDATARANGE_ATTRIBUTES",
    "KSDATARANGE_REQUIRED_ATTRIBUTES",
    "KSEVENT_TYPE_BASICSUPPORT",
    "KSEVENT_TYPE_ENABLE",
    "KSFILTER_NODE",
    "KSJACK_DESCRIPTION2_.*",
    "KSPROPERTY_MEMBER_.*",
    "KSPROPERTY_TYPE_BASICSUPPORT",
    "KSPROPERTY_TYPE_GET",
    "KSPROPERTY_TYPE_SET",
    "PCEVENT_VERB_.*",
    "PCFILTER_NODE",
    "PCPROPERTY_ITEM_FLAG_.*",
    "SPEAKER_FRONT_.*",
    "WAVE_FORMAT_EXTENSIBLE",
    "WAVE_FORMAT_IEEE_FLOAT",
    "WAVE_FORMAT_PCM",
];

/// Kernel Streaming GUIDs: read through their `STATIC_<name>` macros.
const KS_GUIDS: &[&str] = &[
    "AUDIO_SIGNALPROCESSINGMODE_DEFAULT",
    "AUDIO_SIGNALPROCESSINGMODE_RAW",
    "GUID_NULL",
    "KSATTRIBUTEID_AUDIOSIGNALPROCESSING_MODE",
    "KSAUDFNAME_MIC_MUTE",
    "KSAUDFNAME_MIC_VOLUME",
    "KSAUDFNAME_RECORDING_CONTROL",
    "KSCATEGORY_AUDIO",
    "KSCATEGORY_CAPTURE",
    "KSCATEGORY_REALTIME",
    "KSCATEGORY_TOPOLOGY",
    "KSDATAFORMAT_SPECIFIER_NONE",
    "KSDATAFORMAT_SPECIFIER_WAVEFORMATEX",
    "KSDATAFORMAT_SUBTYPE_ANALOG",
    "KSDATAFORMAT_SUBTYPE_IEEE_FLOAT",
    "KSDATAFORMAT_SUBTYPE_PCM",
    "KSDATAFORMAT_TYPE_AUDIO",
    "KSEVENTSETID_PinCapsChange",
    "KSNODETYPE_ADC",
    "KSNODETYPE_MICROPHONE",
    "KSNODETYPE_MICROPHONE_ARRAY",
    "KSNODETYPE_MUTE",
    "KSNODETYPE_VOLUME",
    "KSPROPSETID_Audio",
    "KSPROPSETID_General",
    "KSPROPSETID_Jack",
    "KSPROPSETID_Pin",
    "KSPROPTYPESETID_General",
];

/// PortCls IIDs and CLSIDs: plain `DEFINE_GUID`s, read from their `INITGUID` definitions.
const PORTCLS_GUIDS: &[&str] = &[
    "CLSID_PortTopology",
    "CLSID_PortWaveRT",
    "IID_IAdapterPowerManagement",
    "IID_IMiniport",
    "IID_IMiniportAudioSignalProcessing",
    "IID_IMiniportTopology",
    "IID_IMiniportWaveRT",
    "IID_IMiniportWaveRTInputStream",
    "IID_IMiniportWaveRTStream",
    "IID_IMiniportWaveRTStreamNotification",
    "IID_IPort",
    "IID_IPortEvents",
    "IID_IPortTopology",
    "IID_IPortWaveRT",
    "IID_IPortWaveRTStream",
    "IID_IUnknown",
    "IID_IUnregisterPhysicalConnection",
    "IID_IUnregisterSubdevice",
];

/// Layouts checked against MSVC: type, then the fields whose offsets are compared.
const LAYOUTS: &[(&str, &[&str])] = &[
    (
        "PCFILTER_DESCRIPTOR",
        &[
            "Version",
            "AutomationTable",
            "PinSize",
            "PinCount",
            "Pins",
            "NodeSize",
            "NodeCount",
            "Nodes",
            "ConnectionCount",
            "Connections",
            "CategoryCount",
            "Categories",
        ],
    ),
    (
        "PCPIN_DESCRIPTOR",
        &[
            "MaxGlobalInstanceCount",
            "MaxFilterInstanceCount",
            "MinFilterInstanceCount",
            "AutomationTable",
            "KsPinDescriptor",
        ],
    ),
    (
        "PCNODE_DESCRIPTOR",
        &["Flags", "AutomationTable", "Type", "Name"],
    ),
    (
        "PCCONNECTION_DESCRIPTOR",
        &["FromNode", "FromNodePin", "ToNode", "ToNodePin"],
    ),
    ("PCPROPERTY_ITEM", &["Set", "Id", "Flags", "Handler"]),
    (
        "PCAUTOMATION_TABLE",
        &[
            "PropertyItemSize",
            "PropertyCount",
            "Properties",
            "MethodItemSize",
            "MethodCount",
            "Methods",
            "EventItemSize",
            "EventCount",
            "Events",
            "Reserved",
        ],
    ),
    (
        "PCPROPERTY_REQUEST",
        &[
            "MajorTarget",
            "MinorTarget",
            "Node",
            "PropertyItem",
            "Irp",
            "InstanceSize",
            "Instance",
            "ValueSize",
            "Value",
        ],
    ),
    (
        "KSPIN_DESCRIPTOR",
        &[
            "InterfacesCount",
            "Interfaces",
            "MediumsCount",
            "Mediums",
            "DataRangesCount",
            "DataRanges",
            "DataFlow",
            "Communication",
            "Category",
            "Name",
        ],
    ),
    ("KSDATAFORMAT", &[]),
    ("KSPROPERTY", &[]),
    ("KSP_PIN", &["Property", "PinId"]),
    ("KSMULTIPLE_ITEM", &["Size", "Count"]),
    ("KSATTRIBUTE", &["Size", "Flags", "Attribute"]),
    (
        "KSPROPERTY_DESCRIPTION",
        &[
            "AccessFlags",
            "DescriptionSize",
            "PropTypeSet",
            "MembersListCount",
            "Reserved",
        ],
    ),
    (
        "KSPROPERTY_MEMBERSHEADER",
        &["MembersFlags", "MembersSize", "MembersCount", "Flags"],
    ),
    (
        "KSPROPERTY_STEPPING_LONG",
        &["SteppingDelta", "Reserved", "Bounds"],
    ),
    (
        "KSDATARANGE_AUDIO",
        &[
            "DataRange",
            "MaximumChannels",
            "MinimumBitsPerSample",
            "MaximumBitsPerSample",
            "MinimumSampleFrequency",
            "MaximumSampleFrequency",
        ],
    ),
    (
        "WAVEFORMATEX",
        &[
            "wFormatTag",
            "nChannels",
            "nSamplesPerSec",
            "nAvgBytesPerSec",
            "nBlockAlign",
            "wBitsPerSample",
            "cbSize",
        ],
    ),
    (
        "WAVEFORMATEXTENSIBLE",
        &["Format", "Samples", "dwChannelMask", "SubFormat"],
    ),
    (
        "KSDATAFORMAT_WAVEFORMATEXTENSIBLE",
        &["DataFormat", "WaveFormatExt"],
    ),
    (
        "KSJACK_DESCRIPTION",
        &[
            "ChannelMapping",
            "Color",
            "ConnectionType",
            "GeoLocation",
            "GenLocation",
            "PortConnection",
            "IsConnected",
        ],
    ),
    (
        "KSJACK_DESCRIPTION2",
        &["DeviceStateInfo", "JackCapabilities"],
    ),
    (
        "KSRTAUDIO_HWLATENCY",
        &["FifoSize", "ChipsetDelay", "CodecDelay"],
    ),
    (
        "KSRTAUDIO_HWREGISTER",
        &["Register", "Width", "Numerator", "Denominator", "Accuracy"],
    ),
    ("KSAUDIO_POSITION", &["PlayOffset", "WriteOffset"]),
    // Vtables: a wrong size or slot offset would call the wrong method.
    ("IUnknownVtbl", &["QueryInterface", "AddRef", "Release"]),
    (
        "IMiniportWaveRTVtbl",
        &[
            "QueryInterface",
            "GetDescription",
            "DataRangeIntersection",
            "Init",
            "NewStream",
            "GetDeviceDescription",
        ],
    ),
    (
        "IMiniportTopologyVtbl",
        &["GetDescription", "DataRangeIntersection", "Init"],
    ),
    (
        "IMiniportWaveRTStreamVtbl",
        &[
            "AllocateAudioBuffer",
            "FreeAudioBuffer",
            "GetClockRegister",
            "GetPositionRegister",
            "SetState",
            "GetPosition",
            "SetFormat",
            "GetHWLatency",
        ],
    ),
    (
        "IMiniportWaveRTStreamNotificationVtbl",
        &[
            "AllocateBufferWithNotification",
            "FreeBufferWithNotification",
            "RegisterNotificationEvent",
            "UnregisterNotificationEvent",
        ],
    ),
    ("IMiniportWaveRTInputStreamVtbl", &["GetReadPacket"]),
    ("IMiniportAudioSignalProcessingVtbl", &["GetModes"]),
    (
        "IAdapterPowerManagementVtbl",
        &[
            "PowerChangeState",
            "QueryPowerChangeState",
            "QueryDeviceCapabilities",
        ],
    ),
    ("IPortWaveRTVtbl", &["Init"]),
    ("IPortTopologyVtbl", &["Init"]),
    (
        "IPortWaveRTStreamVtbl",
        &[
            "AllocatePagesForMdl",
            "MapAllocatedPages",
            "UnmapAllocatedPages",
            "FreePagesFromMdl",
        ],
    ),
    ("IUnregisterSubdeviceVtbl", &["UnregisterSubdevice"]),
    (
        "IUnregisterPhysicalConnectionVtbl",
        &["UnregisterPhysicalConnection"],
    ),
];

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    configure_wdk_library_build_and_then(|config| {
        let out_dir = PathBuf::from(env::var("OUT_DIR")?);
        let base = config.bindgen_header_contents([ApiSubset::Base])?;
        generate_bindings(&config, &base, &out_dir)?;
        generate_guids(&config, &base, &out_dir)?;
        generate_layout_checks(&config, &base, &out_dir)?;
        compile_c(&config, &out_dir)?;
        Ok::<(), Box<dyn Error>>(())
    })
}

fn generate_bindings(config: &Config, base: &str, out_dir: &Path) -> Result<(), Box<dyn Error>> {
    let mut builder = bindgen::Builder::wdk_default(config)?
        .header_contents("hostmic-sys-input.h", &format!("{base}{HEADERS}"))
        // Everything not allowlisted comes from `wdk-sys`.
        .allowlist_recursively(false)
        .parse_callbacks(Box::new(WdkSysTypesAreCopy))
        // Only ever handled through pointers, and its full definition drags in the AVStream
        // types.
        .opaque_type("_?KSEVENT_ENTRY")
        .blocklist_type("_KSEVENT_ENTRY__bindgen_.*");
    for function in FUNCTIONS {
        builder = builder.allowlist_function(function);
    }
    for ty in TYPES {
        // Struct tags are spelled `_X`, `tagX`, `_tagX` or `tX` in these headers.
        builder = builder.allowlist_type(format!("(_|tag|_tag|t)?P?{ty}"));
    }
    for var in VARS {
        builder = builder.allowlist_var(var);
    }
    builder
        .generate()?
        .write_to_file(out_dir.join("bindings.rs"))?;
    Ok(())
}

/// A `cc` build for MSVC with the WDK's include paths and preprocessor definitions.
fn msvc_build(config: &Config) -> Result<cc::Build, Box<dyn Error>> {
    let mut build = cc::Build::new();
    build.cargo_metadata(false).warnings(false);
    for include_path in config.include_paths()? {
        build.include(include_path);
    }
    for (key, value) in config.preprocessor_definitions() {
        build.define(&key, value.as_deref());
    }
    Ok(build)
}

/// Compiles the driver's C code for kernel mode: `/kernel` (kernel code generation, no C++
/// exceptions or RTTI), `/GS-` (no security cookie, which would need the kernel's own cookie
/// library) and `/Zl` (no C runtime library named in the object).
fn compile_c(config: &Config, out_dir: &Path) -> Result<(), Box<dyn Error>> {
    let source = Path::new("c").join("map_user.c");
    println!("cargo:rerun-if-changed={}", source.display());
    msvc_build(config)?
        .flag("/kernel")
        .flag("/GS-")
        .flag("/Zl")
        .file(&source)
        .try_compile("hostmic_sys_c")?;
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    Ok(())
}

fn generate_guids(config: &Config, base: &str, out_dir: &Path) -> Result<(), Box<dyn Error>> {
    // With INITGUID, every DEFINE_GUID expands to a definition carrying its value.
    let mut source = format!("#define INITGUID\n{base}{HEADERS}\n");
    for name in KS_GUIDS {
        writeln!(source, "hostmic_guid(\"{name}\", STATIC_{name})")?;
    }
    let input = out_dir.join("guids.c");
    fs::write(&input, source)?;
    let expanded =
        String::from_utf8_lossy(&msvc_build(config)?.file(&input).try_expand()?).into_owned();

    let mut rust = String::from("// Generated by hostmic-sys' build.rs from the WDK headers.\n");
    for name in KS_GUIDS {
        let marker = format!("hostmic_guid(\"{name}\",");
        let start = expanded
            .find(&marker)
            .ok_or_else(|| format!("STATIC_{name} not found"))?
            + marker.len();
        let end = start
            + matching_parenthesis(&expanded[start..])
                .ok_or_else(|| format!("unterminated STATIC_{name}"))?;
        writeln!(rust, "{}", guid_const(name, &expanded[start..end])?)?;
    }
    for name in PORTCLS_GUIDS {
        let value = initguid_value(&expanded, name).ok_or_else(|| format!("{name} not found"))?;
        writeln!(rust, "{}", guid_const(name, &value)?)?;
    }
    fs::write(out_dir.join("guids.rs"), rust)?;
    Ok(())
}

/// Position of the `)` closing an already opened parenthesis.
fn matching_parenthesis(text: &str) -> Option<usize> {
    let mut depth = 0_usize;
    for (index, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => return Some(index),
            ')' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Finds `<name> = { l, w1, w2, { b1, … b8 } }` in the preprocessed output.
fn initguid_value(expanded: &str, name: &str) -> Option<String> {
    let mut rest = expanded;
    while let Some(position) = rest.find(name) {
        let after = &rest[position + name.len()..];
        let preceded_by_identifier = rest[..position]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        let tail = after.trim_start();
        if !preceded_by_identifier && tail.starts_with('=') {
            let body = tail[1..].trim_start().strip_prefix('{')?;
            let end = body.find("} }").or_else(|| body.find("}}"))?;
            return Some(body[..end].replace('{', ","));
        }
        rest = after;
    }
    None
}

/// Turns the 11 comma-separated numbers of a GUID into a Rust constant.
fn guid_const(name: &str, numbers: &str) -> Result<String, Box<dyn Error>> {
    let values = numbers
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(parse_c_sum)
        .collect::<Result<Vec<u64>, _>>()
        .map_err(|error| format!("{name}: {error}"))?;
    let [data1, data2, data3, data4 @ ..] = values.as_slice() else {
        return Err(format!("{name}: expected 11 numbers, got {}", values.len()).into());
    };
    if data4.len() != 8 {
        return Err(format!("{name}: expected 11 numbers, got {}", values.len()).into());
    }
    let data4 = data4
        .iter()
        .map(|byte| format!("{byte:#04x}"))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!(
        "pub const {name}: GUID = GUID {{ Data1: {data1:#010x}, Data2: {data2:#06x}, Data3: \
         {data3:#06x}, Data4: [{data4}] }};"
    ))
}

/// Evaluates a sum of C integer literals, as some `STATIC_*` macros compute one GUID field
/// from another (`0xDFF219E0+(USHORT)(0x0201)`). Anything else is rejected.
fn parse_c_sum(expression: &str) -> Result<u64, String> {
    let mut terms = Vec::new();
    let mut depth = 0_usize;
    let mut start = 0;
    for (index, c) in expression.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '+' if depth == 0 => {
                terms.push(&expression[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    terms.push(&expression[start..]);
    terms.into_iter().try_fold(0_u64, |sum, term| {
        sum.checked_add(parse_c_integer(term)?)
            .ok_or_else(|| format!("overflow in {expression:?}"))
    })
}

/// Parses a C integer literal (`0x73647561L`, `0x0010`, `0`), possibly in parentheses and
/// behind casts such as `(USHORT)(0x0003)`.
fn parse_c_integer(token: &str) -> Result<u64, String> {
    let mut token = token.trim();
    // Drop leading casts: `(` + type name + `)`.
    while let Some(rest) = token.strip_prefix('(') {
        match rest.find(')') {
            Some(end)
                if !rest[..end].is_empty()
                    && rest[..end]
                        .chars()
                        .all(|c| c.is_ascii_alphabetic() || c == '_' || c == ' ') =>
            {
                token = rest[end + 1..].trim_start();
            }
            _ => break,
        }
    }
    let token = token.trim_matches(|c| c == '(' || c == ')' || char::is_whitespace(c));
    let digits = token.trim_end_matches(['L', 'l', 'U', 'u']);
    let parsed = match digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => digits.parse(),
    };
    parsed.map_err(|_| format!("not an integer literal: {token:?}"))
}

fn generate_layout_checks(
    config: &Config,
    base: &str,
    out_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    let mut c = format!("{base}{HEADERS}\n#include <stddef.h>\n");
    let mut rust = String::from("// Generated by hostmic-sys' build.rs.\n");
    for (ty, fields) in LAYOUTS {
        let count = 2 + fields.len();
        write!(
            c,
            "const unsigned long long hostmic_layout_{ty}[{count}] = {{ sizeof({ty}), \
             __alignof({ty})"
        )?;
        for field in *fields {
            write!(c, ", offsetof({ty}, {field})")?;
        }
        c.push_str(" };\n");

        writeln!(
            rust,
            "unsafe extern \"C\" {{ static hostmic_layout_{ty}: [u64; {count}]; }}\n\
             #[test]\n\
             #[allow(non_snake_case)]\n\
             fn layout_{ty}() {{\n\
             \x20   // SAFETY: an immutable array defined in layout.c, compiled into this test.\n\
             \x20   let msvc = unsafe {{ hostmic_layout_{ty} }};\n\
             \x20   assert_eq!(size_of::<{ty}>() as u64, msvc[0], \"size of {ty}\");\n\
             \x20   assert_eq!(align_of::<{ty}>() as u64, msvc[1], \"alignment of {ty}\");"
        )?;
        for (index, field) in fields.iter().enumerate() {
            writeln!(
                rust,
                "    assert_eq!(core::mem::offset_of!({ty}, {field}) as u64, msvc[{}], \
                 \"offset of {ty}::{field}\");",
                index + 2
            )?;
        }
        rust.push_str("}\n");
    }
    let source = out_dir.join("layout.c");
    fs::write(&source, c)?;
    msvc_build(config)?
        .file(&source)
        .compile("hostmic_sys_layout");
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    fs::write(out_dir.join("layout_tests.rs"), rust)?;
    Ok(())
}
