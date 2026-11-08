//! Writes into the capture ring buffer: the buffer the audio engine reads, which the driver
//! fills with the test tone in milestone M3.
//!
//! Frame `n` of the stream lives at byte `(n % ring_frames) * FRAME_BYTES` of the buffer.

use crate::format::{CHANNELS, FRAME_BYTES, SAMPLE_BYTES};
use crate::tone;

/// Writes the tone for the stream frames `first..end` into `ring`.
///
/// - When the range is longer than the ring (the writer was late by more than a whole buffer),
///   only its last `ring_frames` frames are written: the older ones would be overwritten anyway.
/// - An empty or reversed range, or a ring shorter than one frame, writes nothing.
/// - Trailing bytes of `ring` beyond its last whole frame are left untouched.
pub fn write_tone(ring: &mut [u8], first: u64, end: u64) {
    let frames_in_ring = ring.len() / FRAME_BYTES;
    if frames_in_ring == 0 || end <= first {
        return;
    }
    let ring_frames = frames_in_ring as u64;
    let first = first.max(end.saturating_sub(ring_frames));
    let ring = &mut ring[..frames_in_ring * FRAME_BYTES];

    #[allow(
        clippy::cast_possible_truncation,
        reason = "the remainder is below the ring's frame count, a usize"
    )]
    let start = (first % ring_frames) as usize;
    let mut frame = first;
    // At most two runs: from `start` to the end of the ring, then from its beginning.
    let (wrapped, from_start) = ring.split_at_mut(start * FRAME_BYTES);
    for run in [from_start, wrapped] {
        for bytes in run.as_chunks_mut::<FRAME_BYTES>().0 {
            if frame == end {
                return;
            }
            let sample = tone::sample(frame).to_le_bytes();
            for channel in bytes.as_chunks_mut::<SAMPLE_BYTES>().0 {
                *channel = sample;
            }
            frame += 1;
        }
    }
    debug_assert_eq!(frame, end);
}

// The writes above assume interleaved 16-bit samples.
const _: () = assert!(SAMPLE_BYTES == 2 && FRAME_BYTES == CHANNELS as usize * 2);

#[cfg(test)]
mod tests {
    use super::*;

    /// The frame at byte offset `index * FRAME_BYTES`, both channels.
    fn frame_at(ring: &[u8], index: usize) -> [i16; 2] {
        let b = &ring[index * FRAME_BYTES..(index + 1) * FRAME_BYTES];
        [
            i16::from_le_bytes([b[0], b[1]]),
            i16::from_le_bytes([b[2], b[3]]),
        ]
    }

    fn assert_tone(ring: &[u8], index: usize, frame: u64) {
        let s = tone::sample(frame);
        assert_eq!(
            frame_at(ring, index),
            [s, s],
            "ring frame {index}, stream frame {frame}"
        );
    }

    const UNTOUCHED: u8 = 0xAA;

    #[test]
    fn writes_from_the_start() {
        let mut ring = [UNTOUCHED; 100 * FRAME_BYTES];
        write_tone(&mut ring, 0, 60);
        for i in 0..60 {
            assert_tone(&ring, i, i as u64);
        }
        assert!(ring[60 * FRAME_BYTES..].iter().all(|&b| b == UNTOUCHED));
    }

    #[test]
    fn wraps_around() {
        let mut ring = [UNTOUCHED; 100 * FRAME_BYTES];
        // Stream frames 290..330 go to ring frames 90..100 then 0..30.
        write_tone(&mut ring, 290, 330);
        for i in 90..100 {
            assert_tone(&ring, i, 200 + i as u64);
        }
        for i in 0..30 {
            assert_tone(&ring, i, 300 + i as u64);
        }
        assert!(
            ring[30 * FRAME_BYTES..90 * FRAME_BYTES]
                .iter()
                .all(|&b| b == UNTOUCHED)
        );
    }

    #[test]
    fn a_late_writer_keeps_the_last_ring_full() {
        let mut ring = [UNTOUCHED; 100 * FRAME_BYTES];
        write_tone(&mut ring, 0, 1_050);
        for i in 0..100 {
            // Ring frame i holds the latest stream frame below 1050 congruent to i modulo 100.
            let frame = if i < 50 {
                1_000 + i as u64
            } else {
                900 + i as u64
            };
            assert_tone(&ring, i, frame);
        }
    }

    #[test]
    fn empty_and_reversed_ranges_write_nothing() {
        let mut ring = [UNTOUCHED; 10 * FRAME_BYTES];
        write_tone(&mut ring, 5, 5);
        write_tone(&mut ring, 6, 5);
        assert!(ring.iter().all(|&b| b == UNTOUCHED));
    }

    #[test]
    fn partial_frames_are_ignored() {
        let mut short = [UNTOUCHED; FRAME_BYTES - 1];
        write_tone(&mut short, 0, 10);
        assert!(short.iter().all(|&b| b == UNTOUCHED));

        let mut ring = [UNTOUCHED; 3 * FRAME_BYTES + 3];
        write_tone(&mut ring, 0, 10);
        assert!(ring[3 * FRAME_BYTES..].iter().all(|&b| b == UNTOUCHED));
    }

    #[test]
    fn huge_frame_numbers() {
        let mut ring = [UNTOUCHED; 7 * FRAME_BYTES];
        write_tone(&mut ring, u64::MAX - 3, u64::MAX);
        for frame in u64::MAX - 3..u64::MAX {
            assert_tone(&ring, (frame % 7) as usize, frame);
        }
    }
}
