//! Validation of the host interface's requests (PLAN.md, M4).
//!
//! The driver hands each request's input, as the I/O manager copied it, to one of these
//! functions and acts only on what they return: a typed, checked request or the reason for
//! refusing it. Nothing here trusts a length, a value or a combination of values it has not
//! checked, and no input can panic (the fuzz targets check that).
//!
//! The layouts are `hostmic-abi`'s; what the driver accepts is decided here.

use hostmic_abi::{
    ABI_VERSION, AdvanceRequest, AttachRequest, FormatRange, Info, SAMPLE_TYPES_ALL, SampleType,
    State,
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
    /// No event handle.
    NoEvent,
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
    /// The position would exceed `u64::MAX` frames.
    Overflow,
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

/// A checked [`AttachRequest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attach {
    /// The host's event handle value, not 0; the driver still has to reference it.
    pub event: u64,
    /// The formats the microphone offers: those of the host that the driver supports. Never
    /// empty.
    pub formats: FormatRange,
}

/// Decodes and checks the input of [`IOCTL_ATTACH`](hostmic_abi::IOCTL_ATTACH).
///
/// # Errors
///
/// The first problem found, in field order.
pub fn attach(input: &[u8]) -> Result<Attach, Rejection> {
    let request = AttachRequest::from_bytes(exact(input)?);
    if request.abi_version != ABI_VERSION {
        return Err(Rejection::Version {
            received: request.abi_version,
        });
    }
    if request.reserved != 0 {
        return Err(Rejection::Reserved);
    }
    if request.event == 0 {
        return Err(Rejection::NoEvent);
    }
    if !is_valid(&request.formats) {
        return Err(Rejection::InvalidRange);
    }
    let formats = intersect(&request.formats, &SUPPORTED).ok_or(Rejection::NoCommonFormat)?;
    Ok(Attach {
        event: request.event,
        formats,
    })
}

/// Whether `range` holds at least one format and only known sample types.
#[must_use]
pub const fn is_valid(range: &FormatRange) -> bool {
    range.min_rate != 0
        && range.min_rate <= range.max_rate
        && range.min_channels != 0
        && range.min_channels <= range.max_channels
        && range.sample_types != 0
        && range.sample_types & !SAMPLE_TYPES_ALL == 0
}

/// The formats in both `a` and `b`, or `None` when there are none. Both must be valid
/// ([`is_valid`]); the result then is.
#[must_use]
pub fn intersect(a: &FormatRange, b: &FormatRange) -> Option<FormatRange> {
    let range = FormatRange {
        min_rate: a.min_rate.max(b.min_rate),
        max_rate: a.max_rate.min(b.max_rate),
        min_channels: a.min_channels.max(b.min_channels),
        max_channels: a.max_channels.min(b.max_channels),
        sample_types: a.sample_types & b.sample_types,
    };
    is_valid(&range).then_some(range)
}

/// What [`advance`] needs to know about the stream, read by the driver from its atomics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamView {
    /// The stream's generation.
    pub generation: u32,
    /// The stream's state.
    pub state: State,
    /// Frames of the buffer the stream uses.
    pub buffer_frames: u32,
    /// Frames advanced so far.
    pub position: u64,
}

/// Decodes and checks the input of [`IOCTL_ADVANCE`](hostmic_abi::IOCTL_ADVANCE) against the
/// stream; returns the new position.
///
/// The driver then moves the position from `stream.position` to the result with one
/// compare-and-exchange, and refuses the request with [`Rejection::Position`] if another
/// request moved it first.
///
/// # Errors
///
/// The first problem found: size, generation, state, frame count, position, overflow.
pub fn advance(input: &[u8], stream: &StreamView) -> Result<u64, Rejection> {
    let request = AdvanceRequest::from_bytes(exact(input)?);
    if request.generation != stream.generation {
        return Err(Rejection::Generation {
            received: request.generation,
            current: stream.generation,
        });
    }
    if stream.state != State::Running {
        return Err(Rejection::NotRunning);
    }
    if request.frames == 0 || request.frames > stream.buffer_frames {
        return Err(Rejection::Frames {
            received: request.frames,
            buffer: stream.buffer_frames,
        });
    }
    if request.from != stream.position {
        return Err(Rejection::Position {
            received: request.from,
            current: stream.position,
        });
    }
    // At 384 kHz a 64-bit frame count lasts 1.5 million years; refuse rather than wrap anyway.
    stream
        .position
        .checked_add(u64::from(request.frames))
        .ok_or(Rejection::Overflow)
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
        assert!(is_valid(&SUPPORTED));
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
        let attach = attach(&valid_attach().to_bytes()).unwrap();
        assert_eq!(attach.event, 0x1F4);
        assert_eq!(attach.formats, SUPPORTED);
    }

    #[test]
    fn attach_refuses_every_other_size() {
        let bytes = [0; AttachRequest::SIZE + 1];
        for len in 0..=bytes.len() {
            if len == AttachRequest::SIZE {
                continue;
            }
            assert_eq!(
                super::attach(&bytes[..len]),
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
            assert_eq!(attach(&request.to_bytes()), Err(expected));
        };
        check(
            |r| r.abi_version = ABI_VERSION + 1,
            Rejection::Version {
                received: ABI_VERSION + 1,
            },
        );
        check(|r| r.abi_version = 0, Rejection::Version { received: 0 });
        check(|r| r.reserved = 1, Rejection::Reserved);
        check(|r| r.event = 0, Rejection::NoEvent);
        check(|r| r.formats.min_rate = 0, Rejection::InvalidRange);
        check(|r| r.formats.max_rate = 8_000, Rejection::InvalidRange);
        check(|r| r.formats.min_channels = 0, Rejection::InvalidRange);
        check(|r| r.formats.max_channels = 0, Rejection::InvalidRange);
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
    fn intersections() {
        let host = valid_attach().formats;
        assert_eq!(intersect(&host, &SUPPORTED), Some(SUPPORTED));
        assert_eq!(intersect(&SUPPORTED, &host), Some(SUPPORTED));
        assert_eq!(intersect(&host, &host), Some(host));
        let disjoint = FormatRange {
            min_rate: 8_000,
            max_rate: 16_000,
            ..host
        };
        assert_eq!(intersect(&disjoint, &SUPPORTED), None);
    }

    fn running() -> StreamView {
        StreamView {
            generation: 3,
            state: State::Running,
            buffer_frames: 960,
            position: 4_800,
        }
    }

    fn request(generation: u32, frames: u32, from: u64) -> [u8; AdvanceRequest::SIZE] {
        AdvanceRequest {
            generation,
            frames,
            from,
        }
        .to_bytes()
    }

    #[test]
    fn advance_moves_the_position() {
        assert_eq!(advance(&request(3, 96, 4_800), &running()), Ok(4_896));
        assert_eq!(advance(&request(3, 960, 4_800), &running()), Ok(5_760));
        assert_eq!(advance(&request(3, 1, 4_800), &running()), Ok(4_801));
    }

    #[test]
    fn advance_refuses_every_other_size() {
        let bytes = [0; AdvanceRequest::SIZE + 1];
        for len in 0..=bytes.len() {
            if len == AdvanceRequest::SIZE {
                continue;
            }
            assert_eq!(
                advance(&bytes[..len], &running()),
                Err(Rejection::InputSize {
                    received: len,
                    required: AdvanceRequest::SIZE
                })
            );
        }
    }

    #[test]
    fn advance_checks_the_stream() {
        assert_eq!(
            advance(&request(2, 96, 4_800), &running()),
            Err(Rejection::Generation {
                received: 2,
                current: 3
            })
        );
        for state in [State::Stopped, State::Paused] {
            let stream = StreamView { state, ..running() };
            assert_eq!(
                advance(&request(3, 96, 4_800), &stream),
                Err(Rejection::NotRunning)
            );
        }
        for frames in [0, 961, u32::MAX] {
            assert_eq!(
                advance(&request(3, frames, 4_800), &running()),
                Err(Rejection::Frames {
                    received: frames,
                    buffer: 960
                })
            );
        }
        for from in [0, 4_799, 4_801, u64::MAX] {
            assert_eq!(
                advance(&request(3, 96, from), &running()),
                Err(Rejection::Position {
                    received: from,
                    current: 4_800
                })
            );
        }
    }

    #[test]
    fn advance_never_wraps() {
        let stream = StreamView {
            position: u64::MAX - 10,
            ..running()
        };
        assert_eq!(
            advance(&request(3, 10, u64::MAX - 10), &stream),
            Ok(u64::MAX)
        );
        assert_eq!(
            advance(&request(3, 11, u64::MAX - 10), &stream),
            Err(Rejection::Overflow)
        );
    }

    #[test]
    fn a_stream_without_buffer_accepts_no_frames() {
        let stream = StreamView {
            buffer_frames: 0,
            ..running()
        };
        assert_eq!(
            advance(&request(3, 1, 4_800), &stream),
            Err(Rejection::Frames {
                received: 1,
                buffer: 0
            })
        );
    }
}
