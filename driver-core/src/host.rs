//! Validation of the host interface's requests (PLAN.md, M4).
//!
//! The driver hands each request's input, as the I/O manager copied it, to one of these
//! functions and acts only on what they return: a typed, checked request or the reason for
//! refusing it. Nothing here trusts a length, a value or a combination of values it has not
//! checked, and no input can panic (the fuzz targets check that).
//!
//! The layouts are `hostmic-abi`'s; what the driver accepts is decided here.

use hostmic_abi::{
    ABI_VERSION, AdvanceRequest, AttachRequest, FormatRange, Info, Reason, SampleType,
};

use crate::format::{BYTES_PER_SECOND, CHANNELS, SAMPLE_RATE};

/// The formats the driver streams: 48 kHz, 16-bit, stereo (PLAN.md, M4: the ABI carries
/// ranges, the driver accepts one format for now).
pub const SUPPORTED: FormatRange = FormatRange {
    min_rate: SAMPLE_RATE,
    max_rate: SAMPLE_RATE,
    min_channels: CHANNELS,
    max_channels: CHANNELS,
    sample_types: SampleType::Int16.bit(),
};

/// The largest buffer the driver maps into a host: one second of the largest supported
/// format.
pub const MAX_BUFFER_BYTES: u32 = BYTES_PER_SECOND;

/// Why a request was refused. The driver logs it and completes the request with a matching
/// status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejection {
    /// The input is not exactly the request's size (0 for requests without input).
    InputSize {
        /// Bytes received.
        received: usize,
        /// Bytes required.
        required: usize,
    },
    /// The output buffer cannot hold the reply.
    OutputSize {
        /// Bytes available.
        received: usize,
        /// Bytes required.
        required: usize,
    },
    /// The host was built against another ABI version.
    Version {
        /// The host's version.
        received: u32,
    },
    /// A reserved field is not 0.
    Reserved,
    /// The event handle is 0 or not a user-mode handle value.
    Event {
        /// The value received.
        received: u64,
    },
    /// The host's format range is empty, inverted or names an unknown sample type.
    InvalidRange,
    /// No format is in both the host's range and [`SUPPORTED`].
    NoCommonFormat,
    /// The stream is not running.
    NotRunning,
    /// The request was computed for another stream generation.
    Generation {
        /// The request's generation.
        received: u32,
        /// The stream's generation.
        current: u32,
    },
    /// The frame count is 0 or larger than the stream's buffer.
    Frames {
        /// Frames in the request.
        received: u32,
        /// Frames in the stream's buffer.
        buffer: u32,
    },
    /// The stream is not at the position the host expected.
    Position {
        /// The position the host expected.
        received: u64,
        /// The stream's position.
        current: u64,
    },
    /// The position would exceed [`POSITION_MAX`].
    Overflow,
    /// The host is a 32-bit process: the buffer may be mapped above 4 GiB, where it cannot
    /// reach it. Only 64-bit hosts are supported.
    Wow64,
}

impl Rejection {
    /// The [`Reason`] code the driver's counters report for it.
    #[must_use]
    pub const fn reason(self) -> Reason {
        match self {
            Self::InputSize { .. } => Reason::InputSize,
            Self::OutputSize { .. } => Reason::OutputSize,
            Self::Version { .. } => Reason::Version,
            Self::Reserved => Reason::Reserved,
            Self::Event { .. } => Reason::Event,
            Self::InvalidRange => Reason::InvalidRange,
            Self::NoCommonFormat => Reason::NoCommonFormat,
            Self::NotRunning => Reason::NotRunning,
            Self::Generation { .. } => Reason::Generation,
            Self::Frames { .. } => Reason::Frames,
            Self::Position { .. } => Reason::Position,
            Self::Overflow => Reason::Overflow,
            Self::Wow64 => Reason::Wow64,
        }
    }
}

/// What [`IOCTL_GET_INFO`](hostmic_abi::IOCTL_GET_INFO) replies.
#[must_use]
pub const fn info() -> Info {
    Info {
        abi_version: ABI_VERSION,
        flags: 0,
        max_buffer_bytes: MAX_BUFFER_BYTES,
        reserved: 0,
        formats: SUPPORTED,
    }
}

/// Checks the input of a request that takes none.
///
/// # Errors
///
/// [`Rejection::InputSize`] when `input` is not empty.
pub fn no_input(input: &[u8]) -> Result<(), Rejection> {
    exact::<0>(input).map(|_| ())
}

/// Checks that an output buffer of `available` bytes can hold a reply of `required` bytes.
///
/// # Errors
///
/// [`Rejection::OutputSize`] when it cannot.
pub fn output(available: usize, required: usize) -> Result<(), Rejection> {
    if available < required {
        return Err(Rejection::OutputSize {
            received: available,
            required,
        });
    }
    Ok(())
}

/// The largest event handle value accepted. User-mode handle values are small positive
/// numbers; values with bit 31 set, or above 32 bits, are kernel handles (in their 32-bit or
/// sign-extended form) or garbage.
pub const EVENT_MAX: u64 = 0x7FFF_FFFF;

/// A checked [`AttachRequest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attach {
    /// The host's event handle value, in `1..=EVENT_MAX`. The driver still has to reference
    /// it, in the host's process context, with `ObReferenceObjectByHandle(UserMode,
    /// ExEventObjectType, EVENT_MODIFY_STATE)`, which refuses anything but an event handle of
    /// that process with that access.
    pub event: u32,
    /// The formats the microphone offers: those of the host that the driver supports. Never
    /// empty.
    pub formats: FormatRange,
}

/// Decodes and checks the input of [`IOCTL_ATTACH`](hostmic_abi::IOCTL_ATTACH);
/// `caller_is_wow64` is `IoIs32bitProcess` for the calling process.
///
/// # Errors
///
/// [`Rejection::Wow64`] for a 32-bit caller, else the first problem found, in field order.
pub fn attach(input: &[u8], caller_is_wow64: bool) -> Result<Attach, Rejection> {
    if caller_is_wow64 {
        return Err(Rejection::Wow64);
    }
    let request = AttachRequest::from_bytes(exact(input)?);
    if request.abi_version != ABI_VERSION {
        return Err(Rejection::Version {
            received: request.abi_version,
        });
    }
    if request.reserved != 0 {
        return Err(Rejection::Reserved);
    }
    let event = match u32::try_from(request.event) {
        Ok(event) if event != 0 && u64::from(event) <= EVENT_MAX => event,
        _ => {
            return Err(Rejection::Event {
                received: request.event,
            });
        }
    };
    // The range arithmetic is the ABI's, which hosts check with before attaching.
    if !request.formats.is_valid() {
        return Err(Rejection::InvalidRange);
    }
    let formats = request
        .formats
        .intersect(&SUPPORTED)
        .ok_or(Rejection::NoCommonFormat)?;
    Ok(Attach { event, formats })
}

/// The stream's generation and position, packed in one 64-bit word so that the driver reads
/// and updates both with single atomic operations.
///
/// `ADVANCE` requests arrive at any time, concurrently with the stream's state changes and with
/// each other, and the real-time rule forbids a lock. The protocol:
///
/// - **Generations:** an even generation means the stream runs and accepts frames; an odd one
///   means it does not (stopped, paused, or being changed). Every state change or new buffer
///   moves the generation forward, so a request computed for an earlier one is refused.
/// - **Writer** (state changes and buffer allocation, serialized by PortCls): first store the
///   next odd generation ([`Cursor::halted`]), then change the buffer size and the state, then,
///   if the stream runs, store the next even generation ([`Cursor::running`]). The buffer size
///   only changes while the generation is odd.
/// - **`ADVANCE`:** load the word, load the buffer size, call [`advance`], then
///   compare-and-exchange the word from the loaded value to the result. If anything changed in
///   between, the word changed too (its generation or position did), the exchange fails and
///   the request is refused with the current values. Concurrent `ADVANCE`s cannot both win.
///
/// 48 bits of position last 186 years at 48 kHz and 23 years at 384 kHz; 16 bits of generation
/// wrap after 32 768 runs, far more than can happen between an `ADVANCE`'s two atomic
/// operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    /// Even while the stream accepts frames, odd otherwise.
    pub generation: u16,
    /// Frames advanced since the stream started, at most [`POSITION_MAX`].
    pub position: u64,
}

/// The largest position a [`Cursor`] holds.
pub const POSITION_MAX: u64 = (1 << 48) - 1;

impl Cursor {
    /// A new stream: not running, position 0.
    pub const STOPPED: Self = Self {
        generation: 1,
        position: 0,
    };

    /// The packed word: generation in the high 16 bits, position in the low 48.
    #[must_use]
    pub const fn pack(self) -> u64 {
        ((self.generation as u64) << 48) | (self.position & POSITION_MAX)
    }

    /// The cursor of a packed word.
    #[must_use]
    pub const fn unpack(word: u64) -> Self {
        #[allow(clippy::cast_possible_truncation, reason = "the high 16 bits")]
        let generation = (word >> 48) as u16;
        Self {
            generation,
            position: word & POSITION_MAX,
        }
    }

    /// Whether the stream accepts frames.
    #[must_use]
    pub const fn is_running(self) -> bool {
        self.generation.is_multiple_of(2)
    }

    /// The same position under the next odd generation: no more frames accepted.
    #[must_use]
    pub const fn halted(self) -> Self {
        Self {
            generation: self
                .generation
                .wrapping_add(if self.is_running() { 1 } else { 2 }),
            position: self.position,
        }
    }

    /// The same position under the next even generation: frames accepted.
    #[must_use]
    pub const fn running(self) -> Self {
        Self {
            generation: self
                .generation
                .wrapping_add(if self.is_running() { 2 } else { 1 }),
            position: self.position,
        }
    }

    /// Position 0 under the next odd generation: a stopped stream.
    #[must_use]
    pub const fn stopped(self) -> Self {
        Self {
            position: 0,
            ..self.halted()
        }
    }
}

/// Decodes and checks the input of [`IOCTL_ADVANCE`](hostmic_abi::IOCTL_ADVANCE) against the
/// stream's packed [`Cursor`] `word` and the frames of its buffer; returns the new word, which
/// the driver stores with a compare-and-exchange from `word` ([`Cursor`] has the protocol).
///
/// # Errors
///
/// The first problem found: size, generation, state, frame count, position, overflow.
pub fn advance(input: &[u8], word: u64, buffer_frames: u32) -> Result<u64, Rejection> {
    let request = AdvanceRequest::from_bytes(exact(input)?);
    let cursor = Cursor::unpack(word);
    if request.generation != u32::from(cursor.generation) {
        return Err(Rejection::Generation {
            received: request.generation,
            current: u32::from(cursor.generation),
        });
    }
    if !cursor.is_running() {
        return Err(Rejection::NotRunning);
    }
    if request.frames == 0 || request.frames > buffer_frames {
        return Err(Rejection::Frames {
            received: request.frames,
            buffer: buffer_frames,
        });
    }
    if request.from != cursor.position {
        return Err(Rejection::Position {
            received: request.from,
            current: cursor.position,
        });
    }
    let position = cursor.position + u64::from(request.frames);
    if position > POSITION_MAX {
        return Err(Rejection::Overflow);
    }
    Ok(Cursor { position, ..cursor }.pack())
}

/// The access rights that write to an object, which mandatory integrity's "no write up" denies
/// to a lower integrity level: `FILE_WRITE_DATA` (which `ATTACH`, `ADVANCE` and `DETACH` need),
/// `FILE_APPEND_DATA`, `FILE_WRITE_EA`, `FILE_WRITE_ATTRIBUTES`, `DELETE`, `WRITE_DAC`,
/// `WRITE_OWNER`, and the generic rights that include writing (`MAXIMUM_ALLOWED`, `GENERIC_ALL`,
/// `GENERIC_WRITE`).
const WRITE_RIGHTS: u32 = 0x0000_0002
    | 0x0000_0004
    | 0x0000_0010
    | 0x0000_0100
    | 0x0001_0000
    | 0x0004_0000
    | 0x0008_0000
    | 0x0200_0000
    | 0x1000_0000
    | 0x4000_0000;
/// `SECURITY_MANDATORY_MEDIUM_RID`: the integrity level of a standard user's processes.
pub const INTEGRITY_MEDIUM: u32 = 0x2000;

/// Whether `access` (the access masks of a create request, combined: what was asked and what
/// was granted) includes a right that writes.
#[must_use]
pub const fn asks_write(access: u32) -> bool {
    access & WRITE_RIGHTS != 0
}

/// Whether an open of the host interface asking for `access` may proceed from a caller whose
/// tokens' lowest integrity level is `integrity` (`None` when it could not be read).
///
/// Opening for writing is what lets a process attach as the host and feed the microphone: it
/// needs at least medium integrity, as Windows' rule for objects without an integrity label says
/// ("treated as medium", no write up), which a low-integrity process was seen to get past on
/// this device. Reading needs no particular level: the security descriptor decides. An
/// unreadable level refuses writing.
#[must_use]
pub const fn open_allowed(access: u32, integrity: Option<u32>) -> bool {
    if !asks_write(access) {
        return true;
    }
    match integrity {
        Some(level) => level >= INTEGRITY_MEDIUM,
        None => false,
    }
}

/// `input` as an array of exactly `N` bytes.
fn exact<const N: usize>(input: &[u8]) -> Result<&[u8; N], Rejection> {
    input.try_into().map_err(|_| Rejection::InputSize {
        received: input.len(),
        required: N,
    })
}

#[cfg(test)]
mod tests {
    use hostmic_abi::{AdvanceRequest, AttachRequest};

    use super::*;

    fn valid_attach() -> AttachRequest {
        AttachRequest {
            abi_version: ABI_VERSION,
            reserved: 0,
            event: 0x1F4,
            formats: FormatRange {
                min_rate: 44_100,
                max_rate: 96_000,
                min_channels: 1,
                max_channels: 8,
                sample_types: SampleType::Int16.bit() | SampleType::Float32.bit(),
            },
        }
    }

    #[test]
    fn info_reports_the_supported_formats() {
        let info = info();
        assert_eq!(info.abi_version, ABI_VERSION);
        assert_eq!(info.formats, SUPPORTED);
        assert_eq!(info.max_buffer_bytes, 192_000);
        assert!(SUPPORTED.is_valid());
    }

    #[test]
    fn requests_without_input() {
        assert_eq!(no_input(&[]), Ok(()));
        assert_eq!(
            no_input(&[0]),
            Err(Rejection::InputSize {
                received: 1,
                required: 0
            })
        );
    }

    #[test]
    fn output_sizes() {
        assert_eq!(output(0, 0), Ok(()));
        assert_eq!(output(8, 0), Ok(()));
        assert_eq!(output(32, 32), Ok(()));
        assert_eq!(output(64, 32), Ok(()));
        assert_eq!(
            output(31, 32),
            Err(Rejection::OutputSize {
                received: 31,
                required: 32
            })
        );
    }

    #[test]
    fn attach_accepts_a_range_containing_the_supported_format() {
        let attach = attach(&valid_attach().to_bytes(), false).unwrap();
        assert_eq!(attach.event, 0x1F4);
        assert_eq!(attach.formats, SUPPORTED);
    }

    #[test]
    fn attach_refuses_32_bit_hosts() {
        assert_eq!(
            attach(&valid_attach().to_bytes(), true),
            Err(Rejection::Wow64)
        );
    }

    #[test]
    fn attach_refuses_every_other_size() {
        let bytes = [0; AttachRequest::SIZE + 1];
        for len in 0..=bytes.len() {
            if len == AttachRequest::SIZE {
                continue;
            }
            assert_eq!(
                super::attach(&bytes[..len], false),
                Err(Rejection::InputSize {
                    received: len,
                    required: AttachRequest::SIZE
                })
            );
        }
    }

    #[test]
    fn attach_checks_each_field() {
        let check = |change: fn(&mut AttachRequest), expected: Rejection| {
            let mut request = valid_attach();
            change(&mut request);
            assert_eq!(attach(&request.to_bytes(), false), Err(expected));
        };
        check(
            |r| r.abi_version = ABI_VERSION + 1,
            Rejection::Version {
                received: ABI_VERSION + 1,
            },
        );
        check(|r| r.abi_version = 0, Rejection::Version { received: 0 });
        check(|r| r.reserved = 1, Rejection::Reserved);
        for event in [0, 0x8000_0000, 0x1_0000_0004, 0xFFFF_FFFF_8000_1234] {
            let mut request = valid_attach();
            request.event = event;
            assert_eq!(
                attach(&request.to_bytes(), false),
                Err(Rejection::Event { received: event })
            );
        }
        check(|r| r.formats.min_rate = 0, Rejection::InvalidRange);
        check(|r| r.formats.max_rate = 8_000, Rejection::InvalidRange);
        check(|r| r.formats.min_channels = 0, Rejection::InvalidRange);
        check(|r| r.formats.max_channels = 0, Rejection::InvalidRange);
        check(
            |r| {
                r.formats.min_channels = 4;
                r.formats.max_channels = 2;
            },
            Rejection::InvalidRange,
        );
        check(|r| r.formats.sample_types = 0, Rejection::InvalidRange);
        check(
            |r| r.formats.sample_types |= 1 << 4,
            Rejection::InvalidRange,
        );
        check(|r| r.formats.max_rate = 44_100, Rejection::NoCommonFormat);
        check(|r| r.formats.min_channels = 3, Rejection::NoCommonFormat);
        check(
            |r| r.formats.sample_types = SampleType::Float32.bit(),
            Rejection::NoCommonFormat,
        );
    }

    #[test]
    fn the_largest_event_handle_value() {
        let mut request = valid_attach();
        request.event = EVENT_MAX;
        assert_eq!(
            attach(&request.to_bytes(), false).map(|a| a.event),
            Ok(0x7FFF_FFFF)
        );
    }

    #[test]
    fn intersections() {
        let host = valid_attach().formats;
        assert_eq!(host.intersect(&SUPPORTED), Some(SUPPORTED));
        assert_eq!(SUPPORTED.intersect(&host), Some(SUPPORTED));
        assert_eq!(host.intersect(&host), Some(host));
        let disjoint = FormatRange {
            min_rate: 8_000,
            max_rate: 16_000,
            ..host
        };
        assert_eq!(disjoint.intersect(&SUPPORTED), None);
    }

    #[test]
    fn cursor_packing() {
        let cursor = Cursor {
            generation: 0xABCD,
            position: POSITION_MAX,
        };
        assert_eq!(cursor.pack(), 0xABCD_FFFF_FFFF_FFFF);
        assert_eq!(Cursor::unpack(cursor.pack()), cursor);
        assert_eq!(
            Cursor::unpack(0),
            Cursor {
                generation: 0,
                position: 0
            }
        );
        assert_eq!(
            Cursor::unpack(u64::MAX),
            Cursor {
                generation: u16::MAX,
                position: POSITION_MAX
            }
        );
    }

    fn cursor(generation: u16, position: u64) -> Cursor {
        Cursor {
            generation,
            position,
        }
    }

    #[test]
    fn cursor_transitions() {
        let stopped = Cursor::STOPPED;
        assert!(!stopped.is_running());
        let run = stopped.running();
        assert_eq!(run, cursor(2, 0));
        assert!(run.is_running());
        let moved = cursor(2, 960);
        let paused = moved.halted();
        assert_eq!(paused, cursor(3, 960));
        assert_eq!(paused.running(), cursor(4, 960));
        // Halting a halted stream still moves the generation: a new buffer while stopped.
        assert_eq!(paused.halted(), cursor(5, 960));
        assert_eq!(moved.stopped(), cursor(3, 0));
        // Wrapping keeps the parity.
        let last = cursor(u16::MAX - 1, 7);
        assert!(last.is_running());
        assert_eq!(last.halted().generation, u16::MAX);
        assert_eq!(last.halted().running().generation, 0);
        assert_eq!(last.running().generation, 0);
        assert_eq!(cursor(u16::MAX, 0).halted().generation, 1);
    }

    /// A running stream: generation 4, 4 800 frames advanced.
    fn running() -> u64 {
        cursor(4, 4_800).pack()
    }

    fn request(generation: u32, frames: u32, from: u64) -> [u8; AdvanceRequest::SIZE] {
        AdvanceRequest {
            generation,
            frames,
            from,
        }
        .to_bytes()
    }

    fn position(word: u64) -> u64 {
        Cursor::unpack(word).position
    }

    #[test]
    fn advance_moves_the_position() {
        assert_eq!(
            advance(&request(4, 96, 4_800), running(), 960).map(Cursor::unpack),
            Ok(cursor(4, 4_896))
        );
        assert_eq!(
            advance(&request(4, 960, 4_800), running(), 960).map(position),
            Ok(5_760)
        );
        assert_eq!(
            advance(&request(4, 1, 4_800), running(), 960).map(position),
            Ok(4_801)
        );
    }

    #[test]
    fn advance_refuses_every_other_size() {
        let bytes = [0; AdvanceRequest::SIZE + 1];
        for len in 0..=bytes.len() {
            if len == AdvanceRequest::SIZE {
                continue;
            }
            assert_eq!(
                advance(&bytes[..len], running(), 960),
                Err(Rejection::InputSize {
                    received: len,
                    required: AdvanceRequest::SIZE
                })
            );
        }
    }

    #[test]
    fn advance_checks_the_stream() {
        for generation in [2, 6, 0x1_0004] {
            assert_eq!(
                advance(&request(generation, 96, 4_800), running(), 960),
                Err(Rejection::Generation {
                    received: generation,
                    current: 4
                })
            );
        }
        let halted = Cursor::unpack(running()).halted();
        assert_eq!(
            advance(&request(5, 96, 4_800), halted.pack(), 960),
            Err(Rejection::NotRunning)
        );
        for frames in [0, 961, u32::MAX] {
            assert_eq!(
                advance(&request(4, frames, 4_800), running(), 960),
                Err(Rejection::Frames {
                    received: frames,
                    buffer: 960
                })
            );
        }
        for from in [0, 4_799, 4_801, u64::MAX] {
            assert_eq!(
                advance(&request(4, 96, from), running(), 960),
                Err(Rejection::Position {
                    received: from,
                    current: 4_800
                })
            );
        }
    }

    /// A request computed before a stop and a restart names the old generation, even when the
    /// position is back where the host expected it.
    #[test]
    fn advance_after_a_restart_is_refused() {
        let after = cursor(4, 0).stopped().running();
        assert_eq!(after, cursor(6, 0));
        assert_eq!(
            advance(&request(4, 96, 0), after.pack(), 960),
            Err(Rejection::Generation {
                received: 4,
                current: 6
            })
        );
    }

    #[test]
    fn advance_never_exceeds_the_position_range() {
        let word = cursor(4, POSITION_MAX - 10).pack();
        assert_eq!(
            advance(&request(4, 10, POSITION_MAX - 10), word, 960).map(position),
            Ok(POSITION_MAX)
        );
        assert_eq!(
            advance(&request(4, 11, POSITION_MAX - 10), word, 960),
            Err(Rejection::Overflow)
        );
    }

    #[test]
    fn a_stream_without_buffer_accepts_no_frames() {
        assert_eq!(
            advance(&request(4, 1, 4_800), running(), 0),
            Err(Rejection::Frames {
                received: 1,
                buffer: 0
            })
        );
    }

    #[test]
    fn open_needs_medium_integrity_to_write() {
        const LOW: u32 = 0x1000;
        const HIGH: u32 = 0x3000;
        const SYSTEM: u32 = 0x4000;
        assert_eq!(INTEGRITY_MEDIUM, 0x2000);
        // FILE_READ_DATA, FILE_GENERIC_READ, GENERIC_READ, READ_CONTROL, SYNCHRONIZE.
        for read in [
            0x0000_0001,
            0x0012_0089,
            0x8000_0000,
            0x0002_0000,
            0x0010_0000,
        ] {
            assert!(!asks_write(read));
            assert!(open_allowed(read, Some(LOW)));
            assert!(open_allowed(read, None));
        }
        // FILE_WRITE_DATA, FILE_APPEND_DATA, FILE_WRITE_EA, FILE_WRITE_ATTRIBUTES, DELETE,
        // WRITE_DAC, WRITE_OWNER, FILE_GENERIC_WRITE, GENERIC_WRITE, GENERIC_ALL,
        // MAXIMUM_ALLOWED, and read-write.
        for write in [
            0x0000_0002,
            0x0000_0004,
            0x0000_0010,
            0x0000_0100,
            0x0001_0000,
            0x0004_0000,
            0x0008_0000,
            0x0012_0116,
            0x4000_0000,
            0x1000_0000,
            0x0200_0000,
            0xC000_0000,
        ] {
            assert!(asks_write(write));
            assert!(!open_allowed(write, Some(0)));
            assert!(!open_allowed(write, Some(LOW)));
            assert!(!open_allowed(write, Some(0x1FFF)));
            assert!(!open_allowed(write, None));
            assert!(open_allowed(write, Some(0x2000)));
            assert!(open_allowed(write, Some(HIGH)));
            assert!(open_allowed(write, Some(SYSTEM)));
        }
    }
}
