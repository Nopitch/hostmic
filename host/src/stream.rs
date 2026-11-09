//! What a host reads in the stream's state, without system calls.

use hostmic_abi::{Format, SampleType, State, StreamState};

/// Bytes per frame of `format`: channels times the sample's size. `None` for an unknown sample
/// type or no channel (no stream).
#[must_use]
pub fn frame_bytes(format: &Format) -> Option<u32> {
    let sample = SampleType::from_code(format.sample_type)?;
    let bytes = u32::from(format.channels).checked_mul(sample.bytes())?;
    (bytes != 0).then_some(bytes)
}

/// Whether the stream accepts frames now: running, under an even generation, with a buffer.
/// The driver may report the running state one step early, under the last odd generation,
/// which still refuses frames: the host then waits for the next change.
#[must_use]
pub fn accepts_frames(stream: &StreamState) -> bool {
    State::from_code(stream.state) == Some(State::Running)
        && stream.generation.is_multiple_of(2)
        && stream.buffer_frames != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running() -> StreamState {
        StreamState {
            generation: 4,
            state: State::Running.code(),
            format: Format {
                rate: 48_000,
                channels: 2,
                sample_type: SampleType::Int16.code(),
            },
            buffer_frames: 960,
            frames_per_packet: 480,
            position: 0,
        }
    }

    #[test]
    fn frame_sizes() {
        let format = running().format;
        assert_eq!(frame_bytes(&format), Some(4));
        let int24 = Format {
            channels: 6,
            sample_type: SampleType::Int24.code(),
            ..format
        };
        assert_eq!(frame_bytes(&int24), Some(18));
        assert_eq!(
            frame_bytes(&Format::default()),
            None,
            "no stream: no channel"
        );
        let unknown = Format {
            sample_type: 9,
            ..format
        };
        assert_eq!(frame_bytes(&unknown), None);
    }

    #[test]
    fn accepting() {
        assert!(accepts_frames(&running()));
        let odd = StreamState {
            generation: 5,
            ..running()
        };
        assert!(
            !accepts_frames(&odd),
            "running reported early, odd generation"
        );
        let paused = StreamState {
            state: State::Paused.code(),
            ..running()
        };
        assert!(!accepts_frames(&paused));
        let no_buffer = StreamState {
            buffer_frames: 0,
            ..running()
        };
        assert!(!accepts_frames(&no_buffer));
        let unknown = StreamState {
            state: 7,
            ..running()
        };
        assert!(!accepts_frames(&unknown));
    }
}
