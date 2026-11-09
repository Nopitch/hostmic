//! # hostmic-abi
//!
//! The contract between the hostmic driver and the user-mode host that feeds its microphone:
//! the device interface the host opens, the I/O control codes it sends with `DeviceIoControl`,
//! and the exact byte layout of every request and reply.
//!
//! Every structure is `#[repr(C)]`, little-endian, with explicit reserved fields instead of
//! padding, so that a host may pass a pointer to it directly; [`to_bytes`](Info::to_bytes) and
//! `from_bytes` give the same layout without `unsafe`, and the tests check that both agree.
//! The driver decodes inputs with `from_bytes`; what it accepts is decided by
//! `hostmic-driver-core`, not here, except the format range arithmetic
//! ([`FormatRange::is_valid`], [`FormatRange::intersect`]) that a host also needs before it
//! attaches.
//!
//! All requests use `METHOD_BUFFERED`: the I/O manager copies the input into a system buffer
//! once, so the driver never reads the host's memory directly, and never twice.
//!
//! | Request | Input | Output | Access |
//! |---|---|---|---|
//! | [`IOCTL_GET_INFO`] | none | [`Info`] | read |
//! | [`IOCTL_ATTACH`] | [`AttachRequest`] | [`AttachReply`] | read, write |
//! | [`IOCTL_GET_STREAM`] | none | [`StreamState`] | read |
//! | [`IOCTL_ADVANCE`] | [`AdvanceRequest`] | [`AdvanceReply`] | read, write |
//! | [`IOCTL_DETACH`] | none | none | read, write |
//! | [`IOCTL_GET_STATS`] | none | [`Stats`] | read |

#![no_std]

/// A GUID, in Windows' `GUID` layout.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Guid {
    /// `Data1`.
    pub data1: u32,
    /// `Data2`.
    pub data2: u16,
    /// `Data3`.
    pub data3: u16,
    /// `Data4`.
    pub data4: [u8; 8],
}

/// The device interface class of the host interface,
/// `{B2BFAFAE-4F0A-4295-B39B-E6B9F62EE08A}`: hosts list its instances with
/// `CM_Get_Device_Interface_List` and open one with `CreateFile`.
pub const GUID_DEVINTERFACE_HOSTMIC_HOST: Guid = Guid {
    data1: 0xB2BF_AFAE,
    data2: 0x4F0A,
    data3: 0x4295,
    data4: [0xB3, 0x9B, 0xE6, 0xB9, 0xF6, 0x2E, 0xE0, 0x8A],
};

/// The version of this interface. A host sends it in [`AttachRequest`] and the driver reports
/// its own in [`Info`]. Inputs must have their exact size and the version must match, so any
/// change to a code or a layout, even an addition, increments it; a driver that wants to serve
/// older hosts then accepts several versions. Until the first release, version 1 may still
/// change.
pub const ABI_VERSION: u32 = 1;

/// `FILE_DEVICE_UNKNOWN`, the device type of the control codes.
const FILE_DEVICE_UNKNOWN: u32 = 0x22;
/// `METHOD_BUFFERED`.
const METHOD_BUFFERED: u32 = 0;
/// `FILE_READ_DATA`: the handle must have been opened for reading.
const FILE_READ_DATA: u32 = 0x1;
/// `FILE_WRITE_DATA`: the handle must have been opened for writing.
const FILE_WRITE_DATA: u32 = 0x2;

/// `CTL_CODE` of `winioctl.h`.
const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

/// Function codes from 0x800 are reserved for vendors.
const FUNCTION_BASE: u32 = 0x900;

/// Reads the driver's [`Info`]. Any process allowed to open the interface may send it.
pub const IOCTL_GET_INFO: u32 = ctl_code(
    FILE_DEVICE_UNKNOWN,
    FUNCTION_BASE,
    METHOD_BUFFERED,
    FILE_READ_DATA,
);

/// Attaches the calling process as the host: [`AttachRequest`] in, [`AttachReply`] out. One
/// host at a time; the attachment ends with [`IOCTL_DETACH`] from the host's process, when every
/// handle to the host's file object is closed, or when the host's process ends.
pub const IOCTL_ATTACH: u32 = ctl_code(
    FILE_DEVICE_UNKNOWN,
    FUNCTION_BASE + 1,
    METHOD_BUFFERED,
    FILE_READ_DATA | FILE_WRITE_DATA,
);

/// Reads the capture stream's [`StreamState`]. The host sends it when the event it gave in
/// [`AttachRequest`] is signaled; any process allowed to open the interface may send it.
pub const IOCTL_GET_STREAM: u32 = ctl_code(
    FILE_DEVICE_UNKNOWN,
    FUNCTION_BASE + 2,
    METHOD_BUFFERED,
    FILE_READ_DATA,
);

/// Tells the driver that the host wrote more frames: [`AdvanceRequest`] in, [`AdvanceReply`]
/// out. Attached host only (its handle and process). A request computed for a stream state that
/// has changed since (generation, position, not running) fails with `ERROR_NOT_READY`: the host
/// reads [`IOCTL_GET_STREAM`] again. Any other refusal is a host bug or a lost attachment.
///
/// The host writes the frames of a request just before sending it, and keeps requests short
/// (about a millisecond of audio): the driver takes the size of the last request as how far the
/// host may be writing ahead of the position when it hands packets to the audio engine.
/// Requests are serial: the next one starts from the position the last one returned.
pub const IOCTL_ADVANCE: u32 = ctl_code(
    FILE_DEVICE_UNKNOWN,
    FUNCTION_BASE + 3,
    METHOD_BUFFERED,
    FILE_READ_DATA | FILE_WRITE_DATA,
);

/// Ends the attachment: the buffer is unmapped from the host and the microphone shows
/// unplugged. No input, no output. Attached host only (its handle and process): another process
/// holding a duplicate of the host's handle cannot unmap the buffer from under the host.
pub const IOCTL_DETACH: u32 = ctl_code(
    FILE_DEVICE_UNKNOWN,
    FUNCTION_BASE + 4,
    METHOD_BUFFERED,
    FILE_READ_DATA | FILE_WRITE_DATA,
);

/// Reads the driver's counters: [`Stats`]. Any process allowed to open the interface may send
/// it.
pub const IOCTL_GET_STATS: u32 = ctl_code(
    FILE_DEVICE_UNKNOWN,
    FUNCTION_BASE + 5,
    METHOD_BUFFERED,
    FILE_READ_DATA,
);

/// The function number of a control code (bits 2 to 13), as [`Stats::last_request`] reports
/// the request.
#[must_use]
pub const fn function(code: u32) -> u16 {
    ((code >> 2) & 0xFFF) as u16
}

/// The encoding of one sample.
#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SampleType {
    /// Signed 16-bit integer.
    Int16 = 0,
    /// Signed 24-bit integer, packed in 3 bytes.
    Int24 = 1,
    /// Signed 32-bit integer.
    Int32 = 2,
    /// 32-bit IEEE float, nominal range -1.0 to 1.0.
    Float32 = 3,
}

impl SampleType {
    /// Every sample type, in code order.
    pub const ALL: [Self; 4] = [Self::Int16, Self::Int24, Self::Int32, Self::Float32];

    /// The sample type of a wire code, or `None` for an unknown one.
    #[must_use]
    pub const fn from_code(code: u16) -> Option<Self> {
        match code {
            0 => Some(Self::Int16),
            1 => Some(Self::Int24),
            2 => Some(Self::Int32),
            3 => Some(Self::Float32),
            _ => None,
        }
    }

    /// The wire code.
    #[must_use]
    pub const fn code(self) -> u16 {
        self as u16
    }

    /// This type's bit in [`FormatRange::sample_types`].
    #[must_use]
    pub const fn bit(self) -> u32 {
        1 << self.code()
    }

    /// Bytes per sample.
    #[must_use]
    pub const fn bytes(self) -> u32 {
        match self {
            Self::Int16 => 2,
            Self::Int24 => 3,
            Self::Int32 | Self::Float32 => 4,
        }
    }
}

/// Every valid bit of [`FormatRange::sample_types`].
pub const SAMPLE_TYPES_ALL: u32 = SampleType::Int16.bit()
    | SampleType::Int24.bit()
    | SampleType::Int32.bit()
    | SampleType::Float32.bit();

/// One stream format: interleaved frames of `channels` samples.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Format {
    /// Frames per second.
    pub rate: u32,
    /// Samples per frame.
    pub channels: u16,
    /// A [`SampleType`] code.
    pub sample_type: u16,
}

impl Format {
    /// Encoded size in bytes.
    pub const SIZE: usize = 8;

    /// The little-endian encoding.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0; Self::SIZE];
        put_u32(&mut out, 0, self.rate);
        put_u16(&mut out, 4, self.channels);
        put_u16(&mut out, 6, self.sample_type);
        out
    }

    /// Decodes [`to_bytes`](Self::to_bytes)' encoding.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> Self {
        Self {
            rate: get_u32(bytes, 0),
            channels: get_u16(bytes, 4),
            sample_type: get_u16(bytes, 6),
        }
    }
}

/// A set of formats: every combination of a rate, a channel count and a sample type within the
/// bounds.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FormatRange {
    /// Lowest frames per second.
    pub min_rate: u32,
    /// Highest frames per second.
    pub max_rate: u32,
    /// Fewest samples per frame.
    pub min_channels: u16,
    /// Most samples per frame.
    pub max_channels: u16,
    /// One [`SampleType::bit`] per sample type.
    pub sample_types: u32,
}

impl FormatRange {
    /// Encoded size in bytes.
    pub const SIZE: usize = 16;

    /// The little-endian encoding.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0; Self::SIZE];
        put_u32(&mut out, 0, self.min_rate);
        put_u32(&mut out, 4, self.max_rate);
        put_u16(&mut out, 8, self.min_channels);
        put_u16(&mut out, 10, self.max_channels);
        put_u32(&mut out, 12, self.sample_types);
        out
    }

    /// Decodes [`to_bytes`](Self::to_bytes)' encoding.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> Self {
        Self {
            min_rate: get_u32(bytes, 0),
            max_rate: get_u32(bytes, 4),
            min_channels: get_u16(bytes, 8),
            max_channels: get_u16(bytes, 10),
            sample_types: get_u32(bytes, 12),
        }
    }

    /// Whether the range holds at least one format and only known sample types: what the
    /// driver requires of [`AttachRequest::formats`].
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.min_rate != 0
            && self.min_rate <= self.max_rate
            && self.min_channels != 0
            && self.min_channels <= self.max_channels
            && self.sample_types != 0
            && self.sample_types & !SAMPLE_TYPES_ALL == 0
    }

    /// The formats in both ranges, or `None` when there are none. Both must be valid
    /// ([`is_valid`](Self::is_valid)); the result then is.
    #[must_use]
    pub fn intersect(&self, other: &Self) -> Option<Self> {
        let range = Self {
            min_rate: self.min_rate.max(other.min_rate),
            max_rate: self.max_rate.min(other.max_rate),
            min_channels: self.min_channels.max(other.min_channels),
            max_channels: self.max_channels.min(other.max_channels),
            sample_types: self.sample_types & other.sample_types,
        };
        range.is_valid().then_some(range)
    }
}

/// Output of [`IOCTL_GET_INFO`]: what this driver offers.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Info {
    /// The driver's [`ABI_VERSION`].
    pub abi_version: u32,
    /// Reserved, 0.
    pub flags: u32,
    /// The largest buffer the driver maps into a host, in bytes.
    pub max_buffer_bytes: u32,
    /// Reserved, 0.
    pub reserved: u32,
    /// The formats the driver can stream.
    pub formats: FormatRange,
}

impl Info {
    /// Encoded size in bytes.
    pub const SIZE: usize = 32;

    /// The little-endian encoding.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0; Self::SIZE];
        put_u32(&mut out, 0, self.abi_version);
        put_u32(&mut out, 4, self.flags);
        put_u32(&mut out, 8, self.max_buffer_bytes);
        put_u32(&mut out, 12, self.reserved);
        put(&mut out, 16, &self.formats.to_bytes());
        out
    }

    /// Decodes [`to_bytes`](Self::to_bytes)' encoding.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> Self {
        Self {
            abi_version: get_u32(bytes, 0),
            flags: get_u32(bytes, 4),
            max_buffer_bytes: get_u32(bytes, 8),
            reserved: get_u32(bytes, 12),
            formats: FormatRange::from_bytes(&get(bytes, 16)),
        }
    }
}

/// Input of [`IOCTL_ATTACH`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct AttachRequest {
    /// The [`ABI_VERSION`] the host was built against.
    pub abi_version: u32,
    /// Reserved, 0.
    pub reserved: u32,
    /// A handle, in the host's process, to an event the driver signals when the stream state
    /// changes; it needs `EVENT_MODIFY_STATE` access. The handle value, zero-extended. Hosts
    /// are 64-bit processes: the driver refuses 32-bit ones.
    pub event: u64,
    /// The formats the host can write; the microphone offers those the driver also supports.
    pub formats: FormatRange,
}

impl AttachRequest {
    /// Encoded size in bytes.
    pub const SIZE: usize = 32;

    /// The little-endian encoding.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0; Self::SIZE];
        put_u32(&mut out, 0, self.abi_version);
        put_u32(&mut out, 4, self.reserved);
        put_u64(&mut out, 8, self.event);
        put(&mut out, 16, &self.formats.to_bytes());
        out
    }

    /// Decodes [`to_bytes`](Self::to_bytes)' encoding.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> Self {
        Self {
            abi_version: get_u32(bytes, 0),
            reserved: get_u32(bytes, 4),
            event: get_u64(bytes, 8),
            formats: FormatRange::from_bytes(&get(bytes, 16)),
        }
    }
}

/// Output of [`IOCTL_ATTACH`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct AttachReply {
    /// The buffer's address in the host's process. It stays mapped while the host may write into
    /// it: until `DETACH` from the host's process, or until every handle to the host's file
    /// object is closed (or the host's process ends); nothing else undoes it. A host that closes
    /// its handle while its `ATTACH` is in flight receives an address already unmapped.
    pub buffer: u64,
    /// The buffer's size in bytes; a stream uses its first [`StreamState::buffer_frames`]
    /// frames.
    pub buffer_bytes: u32,
    /// Reserved, 0.
    pub reserved: u32,
}

impl AttachReply {
    /// Encoded size in bytes.
    pub const SIZE: usize = 16;

    /// The little-endian encoding.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0; Self::SIZE];
        put_u64(&mut out, 0, self.buffer);
        put_u32(&mut out, 8, self.buffer_bytes);
        put_u32(&mut out, 12, self.reserved);
        out
    }

    /// Decodes [`to_bytes`](Self::to_bytes)' encoding.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> Self {
        Self {
            buffer: get_u64(bytes, 0),
            buffer_bytes: get_u32(bytes, 8),
            reserved: get_u32(bytes, 12),
        }
    }
}

/// The state of the capture stream, as [`StreamState::state`] reports it.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum State {
    /// No stream, or a stopped one: the host writes nothing.
    Stopped = 0,
    /// A stream that is set up but not running (`KSSTATE_ACQUIRE` or `KSSTATE_PAUSE`).
    Paused = 1,
    /// A running stream: the host writes frames and advances the position.
    Running = 2,
}

impl State {
    /// The state of a wire code, or `None` for an unknown one.
    #[must_use]
    pub const fn from_code(code: u32) -> Option<Self> {
        match code {
            0 => Some(Self::Stopped),
            1 => Some(Self::Paused),
            2 => Some(Self::Running),
            _ => None,
        }
    }

    /// The wire code.
    #[must_use]
    pub const fn code(self) -> u32 {
        self as u32
    }
}

/// Output of [`IOCTL_GET_STREAM`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct StreamState {
    /// Changes with every state change or new buffer of the stream; an [`AdvanceRequest`]
    /// names the generation it was computed for, and the driver refuses any other. Compare it
    /// for equality only (the driver uses 16 bits of it, which wrap), except for its parity:
    /// the stream accepts frames only under an even generation. The driver may report
    /// [`State::Running`] one step early, under the last odd generation; a host then waits for
    /// the next change before writing.
    pub generation: u32,
    /// A [`State`] code.
    pub state: u32,
    /// The stream's format; all zeroes without a stream.
    pub format: Format,
    /// Frames of the buffer the stream uses, from its start; 0 without a stream.
    pub buffer_frames: u32,
    /// Frames per packet in event-driven mode, 0 in polling mode.
    pub frames_per_packet: u32,
    /// Frames the host has advanced since the stream started; the next frame goes at
    /// `position % buffer_frames`.
    pub position: u64,
}

impl StreamState {
    /// Encoded size in bytes.
    pub const SIZE: usize = 32;

    /// The little-endian encoding.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0; Self::SIZE];
        put_u32(&mut out, 0, self.generation);
        put_u32(&mut out, 4, self.state);
        put(&mut out, 8, &self.format.to_bytes());
        put_u32(&mut out, 16, self.buffer_frames);
        put_u32(&mut out, 20, self.frames_per_packet);
        put_u64(&mut out, 24, self.position);
        out
    }

    /// Decodes [`to_bytes`](Self::to_bytes)' encoding.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> Self {
        Self {
            generation: get_u32(bytes, 0),
            state: get_u32(bytes, 4),
            format: Format::from_bytes(&get(bytes, 8)),
            buffer_frames: get_u32(bytes, 16),
            frames_per_packet: get_u32(bytes, 20),
            position: get_u64(bytes, 24),
        }
    }
}

/// Input of [`IOCTL_ADVANCE`]: the host wrote `frames` frames from position `from`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct AdvanceRequest {
    /// The [`StreamState::generation`] the host wrote for.
    pub generation: u32,
    /// Frames written, at least 1 and at most the stream's buffer.
    pub frames: u32,
    /// The position the host expects the stream to be at, as [`StreamState::position`] or the
    /// last [`AdvanceReply::position`] reported it.
    pub from: u64,
}

impl AdvanceRequest {
    /// Encoded size in bytes.
    pub const SIZE: usize = 16;

    /// The little-endian encoding.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0; Self::SIZE];
        put_u32(&mut out, 0, self.generation);
        put_u32(&mut out, 4, self.frames);
        put_u64(&mut out, 8, self.from);
        out
    }

    /// Decodes [`to_bytes`](Self::to_bytes)' encoding.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> Self {
        Self {
            generation: get_u32(bytes, 0),
            frames: get_u32(bytes, 4),
            from: get_u64(bytes, 8),
        }
    }
}

/// Output of [`IOCTL_ADVANCE`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct AdvanceReply {
    /// The stream position after the request.
    pub position: u64,
}

impl AdvanceReply {
    /// Encoded size in bytes.
    pub const SIZE: usize = 8;

    /// The little-endian encoding.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        self.position.to_le_bytes()
    }

    /// Decodes [`to_bytes`](Self::to_bytes)' encoding.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> Self {
        Self {
            position: u64::from_le_bytes(*bytes),
        }
    }
}

/// Why the driver refused a request, as [`Stats::last_reason`] reports it.
#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reason {
    /// No request refused yet.
    None = 0,
    /// The input is not exactly the request's size.
    InputSize = 1,
    /// The output buffer cannot hold the reply.
    OutputSize = 2,
    /// The host was built against another [`ABI_VERSION`].
    Version = 3,
    /// A reserved field is not 0.
    Reserved = 4,
    /// The event handle value is invalid.
    Event = 5,
    /// The host's format range is empty, inverted or names an unknown sample type.
    InvalidRange = 6,
    /// No format is both in the host's range and supported.
    NoCommonFormat = 7,
    /// `ADVANCE`: the stream is not running.
    NotRunning = 8,
    /// `ADVANCE`: computed for another stream generation.
    Generation = 9,
    /// `ADVANCE`: 0 frames, or more than the stream's buffer.
    Frames = 10,
    /// `ADVANCE`: the stream is not at the position the host expected.
    Position = 11,
    /// `ADVANCE`: the position would overflow.
    Overflow = 12,
    /// `ATTACH`: a 32-bit host.
    Wow64 = 13,
    /// The request must come from the attached host's handle and process.
    NotHost = 14,
    /// Refused by the driver's state or a kernel call: [`Stats::last_status`] says which.
    Status = 15,
}

impl Reason {
    /// Every reason, in code order.
    pub const ALL: [Self; 16] = [
        Self::None,
        Self::InputSize,
        Self::OutputSize,
        Self::Version,
        Self::Reserved,
        Self::Event,
        Self::InvalidRange,
        Self::NoCommonFormat,
        Self::NotRunning,
        Self::Generation,
        Self::Frames,
        Self::Position,
        Self::Overflow,
        Self::Wow64,
        Self::NotHost,
        Self::Status,
    ];

    /// The reason of a wire code, or `None` for an unknown one.
    #[must_use]
    pub const fn from_code(code: u16) -> Option<Self> {
        if (code as usize) < Self::ALL.len() {
            Some(Self::ALL[code as usize])
        } else {
            None
        }
    }

    /// The wire code.
    #[must_use]
    pub const fn code(self) -> u16 {
        self as u16
    }
}

/// Output of [`IOCTL_GET_STATS`]: the counters of one host interface (one microphone), since
/// its device started, never reset. Each counter is read on its own: they are consistent with
/// one another only within a few requests. Any process allowed to open the interface reads
/// them, as it reads the stream's state.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Stats {
    /// Frames the host advanced.
    pub frames_advanced: u64,
    /// Packets the host's advances completed (event-driven streams only); the audio engine's
    /// event is signaled for them when it registered one.
    pub packets_completed: u64,
    /// Packets the audio engine never read: when it asked for the next packet, the host had
    /// already overwritten it (it got more than a buffer ahead of the engine). Counted when the
    /// engine reads again.
    pub packets_skipped: u64,
    /// Requests refused, of any kind and from any process allowed to open the interface.
    pub requests_refused: u64,
    /// The [`function`] of the last refused request's control code; 0 if none.
    pub last_request: u16,
    /// Why it was refused: a [`Reason`] code.
    pub last_reason: u16,
    /// The status it completed with (an `NTSTATUS`).
    pub last_status: u32,
}

impl Stats {
    /// Encoded size in bytes.
    pub const SIZE: usize = 40;

    /// The little-endian encoding.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0; Self::SIZE];
        put_u64(&mut out, 0, self.frames_advanced);
        put_u64(&mut out, 8, self.packets_completed);
        put_u64(&mut out, 16, self.packets_skipped);
        put_u64(&mut out, 24, self.requests_refused);
        put(&mut out, 32, &self.last_request.to_le_bytes());
        put(&mut out, 34, &self.last_reason.to_le_bytes());
        put_u32(&mut out, 36, self.last_status);
        out
    }

    /// Decodes [`to_bytes`](Self::to_bytes)' encoding.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> Self {
        Self {
            frames_advanced: get_u64(bytes, 0),
            packets_completed: get_u64(bytes, 8),
            packets_skipped: get_u64(bytes, 16),
            requests_refused: get_u64(bytes, 24),
            last_request: u16::from_le_bytes(get(bytes, 32)),
            last_reason: u16::from_le_bytes(get(bytes, 34)),
            last_status: get_u32(bytes, 36),
        }
    }
}

// Fixed-offset little-endian accessors. Every call site uses a constant offset that the
// tests exercise, so the slicing below cannot go out of bounds.

fn put(out: &mut [u8], at: usize, bytes: &[u8]) {
    out[at..at + bytes.len()].copy_from_slice(bytes);
}

fn put_u16(out: &mut [u8], at: usize, value: u16) {
    put(out, at, &value.to_le_bytes());
}

fn put_u32(out: &mut [u8], at: usize, value: u32) {
    put(out, at, &value.to_le_bytes());
}

fn put_u64(out: &mut [u8], at: usize, value: u64) {
    put(out, at, &value.to_le_bytes());
}

fn get<const N: usize>(bytes: &[u8], at: usize) -> [u8; N] {
    let mut out = [0; N];
    out.copy_from_slice(&bytes[at..at + N]);
    out
}

fn get_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(get(bytes, at))
}

fn get_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(get(bytes, at))
}

fn get_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(get(bytes, at))
}

#[cfg(test)]
mod tests {
    use core::mem::{offset_of, size_of};

    use super::*;

    /// The control codes as `winioctl.h`'s `CTL_CODE` computes them.
    #[test]
    fn control_codes() {
        assert_eq!(IOCTL_GET_INFO, 0x0022_6400);
        assert_eq!(IOCTL_ATTACH, 0x0022_E404);
        assert_eq!(IOCTL_GET_STREAM, 0x0022_6408);
        assert_eq!(IOCTL_ADVANCE, 0x0022_E40C);
        assert_eq!(IOCTL_DETACH, 0x0022_E410);
        assert_eq!(IOCTL_GET_STATS, 0x0022_6414);
        assert_eq!(function(IOCTL_GET_INFO), 0x900);
        assert_eq!(function(IOCTL_GET_STATS), 0x905);
    }

    #[test]
    fn reasons() {
        for (code, reason) in Reason::ALL.into_iter().enumerate() {
            assert_eq!(usize::from(reason.code()), code);
            assert_eq!(Reason::from_code(reason.code()), Some(reason));
        }
        assert_eq!(Reason::from_code(16), None);
        assert_eq!(Reason::from_code(u16::MAX), None);
    }

    #[test]
    fn interface_guid() {
        assert_eq!(size_of::<Guid>(), 16);
        // {B2BFAFAE-4F0A-4295-B39B-E6B9F62EE08A}
        let g = GUID_DEVINTERFACE_HOSTMIC_HOST;
        assert_eq!((g.data1, g.data2, g.data3), (0xB2BF_AFAE, 0x4F0A, 0x4295));
        assert_eq!(g.data4, [0xB3, 0x9B, 0xE6, 0xB9, 0xF6, 0x2E, 0xE0, 0x8A]);
    }

    #[test]
    fn sample_types() {
        for sample_type in SampleType::ALL {
            assert_eq!(SampleType::from_code(sample_type.code()), Some(sample_type));
            assert_eq!(SAMPLE_TYPES_ALL & sample_type.bit(), sample_type.bit());
        }
        assert_eq!(SampleType::from_code(4), None);
        assert_eq!(SAMPLE_TYPES_ALL, 0xF);
    }

    #[test]
    fn states() {
        for state in [State::Stopped, State::Paused, State::Running] {
            assert_eq!(State::from_code(state.code()), Some(state));
        }
        assert_eq!(State::from_code(3), None);
    }

    /// The in-memory layout a host passes by pointer is the byte encoding: same size, no
    /// padding, fields at the offsets the encoders use.
    #[test]
    fn layouts_match_encodings() {
        assert_eq!(size_of::<Format>(), Format::SIZE);
        assert_eq!(offset_of!(Format, channels), 4);
        assert_eq!(offset_of!(Format, sample_type), 6);

        assert_eq!(size_of::<FormatRange>(), FormatRange::SIZE);
        assert_eq!(offset_of!(FormatRange, max_rate), 4);
        assert_eq!(offset_of!(FormatRange, min_channels), 8);
        assert_eq!(offset_of!(FormatRange, max_channels), 10);
        assert_eq!(offset_of!(FormatRange, sample_types), 12);

        assert_eq!(size_of::<Info>(), Info::SIZE);
        assert_eq!(offset_of!(Info, flags), 4);
        assert_eq!(offset_of!(Info, max_buffer_bytes), 8);
        assert_eq!(offset_of!(Info, reserved), 12);
        assert_eq!(offset_of!(Info, formats), 16);

        assert_eq!(size_of::<AttachRequest>(), AttachRequest::SIZE);
        assert_eq!(offset_of!(AttachRequest, reserved), 4);
        assert_eq!(offset_of!(AttachRequest, event), 8);
        assert_eq!(offset_of!(AttachRequest, formats), 16);

        assert_eq!(size_of::<AttachReply>(), AttachReply::SIZE);
        assert_eq!(offset_of!(AttachReply, buffer_bytes), 8);
        assert_eq!(offset_of!(AttachReply, reserved), 12);

        assert_eq!(size_of::<StreamState>(), StreamState::SIZE);
        assert_eq!(offset_of!(StreamState, state), 4);
        assert_eq!(offset_of!(StreamState, format), 8);
        assert_eq!(offset_of!(StreamState, buffer_frames), 16);
        assert_eq!(offset_of!(StreamState, frames_per_packet), 20);
        assert_eq!(offset_of!(StreamState, position), 24);

        assert_eq!(size_of::<AdvanceRequest>(), AdvanceRequest::SIZE);
        assert_eq!(offset_of!(AdvanceRequest, frames), 4);
        assert_eq!(offset_of!(AdvanceRequest, from), 8);

        assert_eq!(size_of::<AdvanceReply>(), AdvanceReply::SIZE);

        assert_eq!(size_of::<Stats>(), Stats::SIZE);
        assert_eq!(offset_of!(Stats, packets_completed), 8);
        assert_eq!(offset_of!(Stats, packets_skipped), 16);
        assert_eq!(offset_of!(Stats, requests_refused), 24);
        assert_eq!(offset_of!(Stats, last_request), 32);
        assert_eq!(offset_of!(Stats, last_reason), 34);
        assert_eq!(offset_of!(Stats, last_status), 36);
    }

    /// Distinct bytes in every field, so that a swapped or misplaced field shows.
    fn range() -> FormatRange {
        FormatRange {
            min_rate: 0x0102_0304,
            max_rate: 0x0506_0708,
            min_channels: 0x090A,
            max_channels: 0x0B0C,
            sample_types: 0x0D0E_0F10,
        }
    }

    #[test]
    fn round_trips() {
        let format = Format {
            rate: 0x1122_3344,
            channels: 0x5566,
            sample_type: 0x7788,
        };
        assert_eq!(Format::from_bytes(&format.to_bytes()), format);
        assert_eq!(
            format.to_bytes(),
            [0x44, 0x33, 0x22, 0x11, 0x66, 0x55, 0x88, 0x77]
        );

        assert_eq!(FormatRange::from_bytes(&range().to_bytes()), range());

        let info = Info {
            abi_version: 1,
            flags: 2,
            max_buffer_bytes: 3,
            reserved: 4,
            formats: range(),
        };
        assert_eq!(Info::from_bytes(&info.to_bytes()), info);

        let attach = AttachRequest {
            abi_version: 1,
            reserved: 2,
            event: 0x1122_3344_5566_7788,
            formats: range(),
        };
        assert_eq!(AttachRequest::from_bytes(&attach.to_bytes()), attach);
        assert_eq!(attach.to_bytes()[8], 0x88);

        let reply = AttachReply {
            buffer: 0x0000_7FF0_1234_0000,
            buffer_bytes: 192_000,
            reserved: 0,
        };
        assert_eq!(AttachReply::from_bytes(&reply.to_bytes()), reply);

        let stream = StreamState {
            generation: 7,
            state: State::Running.code(),
            format: Format {
                rate: 48_000,
                channels: 2,
                sample_type: SampleType::Int16.code(),
            },
            buffer_frames: 960,
            frames_per_packet: 480,
            position: u64::MAX - 1,
        };
        assert_eq!(StreamState::from_bytes(&stream.to_bytes()), stream);

        let advance = AdvanceRequest {
            generation: 7,
            frames: 96,
            from: 0x0102_0304_0506_0708,
        };
        assert_eq!(AdvanceRequest::from_bytes(&advance.to_bytes()), advance);

        let advanced = AdvanceReply { position: 42 };
        assert_eq!(AdvanceReply::from_bytes(&advanced.to_bytes()), advanced);

        let stats = Stats {
            frames_advanced: 0x0102_0304_0506_0708,
            packets_completed: 0x1112_1314_1516_1718,
            packets_skipped: 0x2122_2324_2526_2728,
            requests_refused: 0x3132_3334_3536_3738,
            last_request: 0x4142,
            last_reason: 0x4344,
            last_status: 0xC000_000D,
        };
        assert_eq!(Stats::from_bytes(&stats.to_bytes()), stats);
        assert_eq!(stats.to_bytes()[32..36], [0x42, 0x41, 0x44, 0x43]);
    }
}
