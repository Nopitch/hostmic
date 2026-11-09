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
    for (index, word) in tone_words(first, end, frames_in_ring) {
        let at = index * FRAME_BYTES;
        ring[at..at + FRAME_BYTES].copy_from_slice(&word.to_le_bytes());
    }
}

/// The tone for the stream frames `first..end` in a ring of `ring_frames` frames, as the driver
/// writes it into memory it shares with user mode (one store per frame): for each frame, its
/// index in the ring and the frame itself, both channels in one little-endian 32-bit word.
///
/// Same rules as [`write_tone`]: only the last `ring_frames` frames of a longer range, nothing
/// for an empty or reversed range or an empty ring. No input can panic or overflow.
pub fn tone_words(first: u64, end: u64, ring_frames: usize) -> impl Iterator<Item = (usize, u32)> {
    let ring = ring_frames as u64;
    let first = if ring == 0 {
        end
    } else {
        first.max(end.saturating_sub(ring))
    };
    (first..end).map(move |frame| {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the remainder is below the ring's frame count, a usize"
        )]
        let index = (frame % ring) as usize;
        let sample = u32::from(tone::sample(frame).cast_unsigned());
        (index, sample | (sample << 16))
    })
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
    fn tone_words_without_a_ring_yield_nothing() {
        assert_eq!(tone_words(0, 10, 0).count(), 0);
        assert_eq!(tone_words(u64::MAX - 1, u64::MAX, 0).count(), 0);
        assert_eq!(tone_words(10, 0, 4).count(), 0);
    }

    #[test]
    fn tone_words_are_both_channels_of_a_frame() {
        for (frame, (index, word)) in (1_000..1_010).zip(tone_words(1_000, 1_010, 960)) {
            assert_eq!(index as u64, frame % 960);
            let sample = tone::sample(frame).to_le_bytes();
            let bytes = word.to_le_bytes();
            assert_eq!(bytes[..2], sample, "left channel of frame {frame}");
            assert_eq!(bytes[2..], sample, "right channel of frame {frame}");
        }
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
