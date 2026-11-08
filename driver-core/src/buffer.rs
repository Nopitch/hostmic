//! The geometry of a stream's buffer within the buffer shared with the host (PLAN.md, M4).
//!
//! The driver owns one buffer for the whole attachment of a host. When the audio engine asks
//! for a stream buffer (`AllocateAudioBuffer`, `AllocateBufferWithNotification`), the stream
//! uses the start of that buffer. The engine requires the size it gets to be at least the size
//! it asked for, or it does not use the buffer; so the size is rounded up, never down.

/// Where a stream's frames live in the shared buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// Frames the stream uses, from the start of the shared buffer.
    pub frames: u32,
    /// The same in bytes: what the driver reports as the buffer's actual size.
    pub bytes: u32,
    /// Frames per packet in event-driven mode, 0 in polling mode.
    pub frames_per_packet: u32,
}

/// The geometry of a stream that asked for `requested` bytes split into `packets` packets (0
/// in polling mode), with frames of `frame_bytes` bytes, in a shared buffer of `capacity`
/// bytes.
///
/// The size is rounded up to whole frames and, in event-driven mode, to whole packets of
/// whole frames. Returns `None` when a size is 0 or the result does not fit in `capacity`; no
/// input can panic or overflow.
#[must_use]
pub const fn geometry(
    requested: u32,
    packets: u32,
    frame_bytes: u32,
    capacity: u32,
) -> Option<Geometry> {
    if requested == 0 || frame_bytes == 0 {
        return None;
    }
    let mut frames = requested.div_ceil(frame_bytes);
    let mut frames_per_packet = 0;
    if packets != 0 {
        frames_per_packet = frames.div_ceil(packets);
        frames = match frames_per_packet.checked_mul(packets) {
            Some(frames) => frames,
            None => return None,
        };
    }
    let Some(bytes) = frames.checked_mul(frame_bytes) else {
        return None;
    };
    if bytes > capacity {
        return None;
    }
    Some(Geometry {
        frames,
        bytes,
        frames_per_packet,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 48 kHz, 16-bit stereo, one second.
    const FRAME: u32 = 4;
    const CAPACITY: u32 = 192_000;

    #[test]
    fn polling_mode_rounds_up_to_whole_frames() {
        assert_eq!(
            geometry(3_840, 0, FRAME, CAPACITY),
            Some(Geometry {
                frames: 960,
                bytes: 3_840,
                frames_per_packet: 0
            })
        );
        assert_eq!(
            geometry(3_841, 0, FRAME, CAPACITY),
            Some(Geometry {
                frames: 961,
                bytes: 3_844,
                frames_per_packet: 0
            })
        );
        assert_eq!(geometry(1, 0, FRAME, CAPACITY).unwrap().bytes, 4);
    }

    /// The audio engine's shared-mode request: two packets of 10 ms.
    #[test]
    fn event_mode_splits_into_equal_packets() {
        assert_eq!(
            geometry(3_840, 2, FRAME, CAPACITY),
            Some(Geometry {
                frames: 960,
                bytes: 3_840,
                frames_per_packet: 480
            })
        );
        // 961 frames in 2 packets: 481 frames each.
        assert_eq!(
            geometry(3_844, 2, FRAME, CAPACITY),
            Some(Geometry {
                frames: 962,
                bytes: 3_848,
                frames_per_packet: 481
            })
        );
        // More packets than frames: one frame per packet.
        assert_eq!(
            geometry(4, 3, FRAME, CAPACITY),
            Some(Geometry {
                frames: 3,
                bytes: 12,
                frames_per_packet: 1
            })
        );
    }

    #[test]
    fn the_result_is_never_smaller_than_the_request() {
        for requested in 1..=2_000 {
            for packets in 0..=4 {
                let g = geometry(requested, packets, 6, CAPACITY).unwrap();
                assert!(g.bytes >= requested);
                assert_eq!(g.bytes, g.frames * 6);
                if packets != 0 {
                    assert_eq!(g.frames, g.frames_per_packet * packets);
                }
            }
        }
    }

    #[test]
    fn capacity_is_a_hard_limit() {
        assert_eq!(
            geometry(CAPACITY, 0, FRAME, CAPACITY).unwrap().bytes,
            CAPACITY
        );
        assert_eq!(geometry(CAPACITY + 1, 0, FRAME, CAPACITY), None);
        // Rounding to packets may push a request that fits over the capacity: 48 000 frames
        // in 7 packets of 6 858 frames.
        assert_eq!(geometry(CAPACITY, 7, FRAME, CAPACITY), None);
    }

    #[test]
    fn zero_sizes_and_extremes_do_not_panic() {
        assert_eq!(geometry(0, 2, FRAME, CAPACITY), None);
        assert_eq!(geometry(3_840, 2, 0, CAPACITY), None);
        assert_eq!(
            geometry(u32::MAX, u32::MAX, 1, u32::MAX).map(|g| g.frames),
            Some(u32::MAX)
        );
        assert_eq!(geometry(u32::MAX, 2, 1, u32::MAX), None);
        assert_eq!(
            geometry(u32::MAX, 0, u32::MAX, u32::MAX).map(|g| g.frames),
            Some(1)
        );
        assert_eq!(geometry(u32::MAX, 0, 2, u32::MAX), None);
    }
}
