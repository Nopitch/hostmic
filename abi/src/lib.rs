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
//! `hostmic-driver-core`, not here.
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
/// its own in [`Info`]; any incompatible change to a code or a layout increments it.
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
/// host at a time; the attachment ends with [`IOCTL_DETACH`] or when the handle is closed.
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
/// out. Attached host only.
pub const IOCTL_ADVANCE: u32 = ctl_code(
    FILE_DEVICE_UNKNOWN,
    FUNCTION_BASE + 3,
    METHOD_BUFFERED,
    FILE_READ_DATA | FILE_WRITE_DATA,
);

/// Ends the attachment: the buffer is unmapped from the host and the microphone shows
/// unplugged. No input, no output. Attached host only.
pub const IOCTL_DETACH: u32 = ctl_code(
    FILE_DEVICE_UNKNOWN,
    FUNCTION_BASE + 4,
    METHOD_BUFFERED,
    FILE_READ_DATA | FILE_WRITE_DATA,
);

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
    /// changes; it needs `EVENT_MODIFY_STATE` access. The handle value, zero-extended.
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
    /// The buffer's address in the host's process; it stays mapped until the host detaches.
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
    /// Increments each time a stream gets a new buffer geometry or starts from position 0; an
    /// [`AdvanceRequest`] names the generation it was computed for.
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
    use core::mem::{align_of, offset_of, size_of};

    use super::*;

    /// The control codes as `winioctl.h`'s `CTL_CODE` computes them.
    #[test]
    fn control_codes() {
        assert_eq!(IOCTL_GET_INFO, 0x0022_6400);
        assert_eq!(IOCTL_ATTACH, 0x0022_E404);
        assert_eq!(IOCTL_GET_STREAM, 0x0022_6408);
        assert_eq!(IOCTL_ADVANCE, 0x0022_E40C);
        assert_eq!(IOCTL_DETACH, 0x0022_E410);
    }

    #[test]
    fn interface_guid() {
        assert_eq!(size_of::<Guid>(), 16);
        assert_eq!(GUID_DEVINTERFACE_HOSTMIC_HOST.data1, 0xB2BF_AFAE);
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
        assert_eq!(align_of::<AttachRequest>(), 8);
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
    }
}
