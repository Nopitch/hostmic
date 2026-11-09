//! Where frames go in the stream's ring buffer, without system calls.

use core::ops::Range;

/// The byte ranges of the ring that `frames` frames written from stream position `from` fill,
/// in order: the part up to the ring's end, then the part that wraps to its start (empty when
/// nothing wraps). The ring holds `ring_frames` frames of `frame_bytes` bytes; frame `n` of the
/// stream goes at `n % ring_frames`.
///
/// `None` when the write is empty, larger than the ring, or the ring is empty or does not fit
/// the address space.
#[must_use]
pub fn segments(
    from: u64,
    frames: u32,
    ring_frames: u32,
    frame_bytes: u32,
) -> Option<[Range<usize>; 2]> {
    if frames == 0 || ring_frames == 0 || frame_bytes == 0 || frames > ring_frames {
        return None;
    }
    // Below `ring_frames`, a `u32`.
    let start = u32::try_from(from % u64::from(ring_frames)).ok()?;
    let first = frames.min(ring_frames - start);
    let second = frames - first;
    let bytes = |frame: u32| usize::try_from(u64::from(frame) * u64::from(frame_bytes)).ok();
    // The ring's size in bytes must fit before any part of it is named.
    bytes(ring_frames)?;
    Some([bytes(start)?..bytes(start + first)?, 0..bytes(second)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_the_ring() {
        assert_eq!(segments(0, 48, 960, 4), Some([0..192, 0..0]));
        assert_eq!(segments(100, 48, 960, 4), Some([400..592, 0..0]));
        // Up to the ring's last byte exactly.
        assert_eq!(segments(912, 48, 960, 4), Some([3648..3840, 0..0]));
    }

    #[test]
    fn wrapping() {
        assert_eq!(segments(940, 48, 960, 4), Some([3760..3840, 0..112]));
        // Positions past the first lap.
        assert_eq!(
            segments(960 * 7 + 940, 48, 960, 4),
            Some([3760..3840, 0..112])
        );
        // A whole ring from its middle.
        assert_eq!(segments(480, 960, 960, 4), Some([1920..3840, 0..1920]));
    }

    #[test]
    fn far_positions() {
        let from = u64::MAX - 10;
        let start = u32::try_from(from % 960).unwrap();
        let [first, second] = segments(from, 48, 960, 4).unwrap();
        assert_eq!(first.start, start as usize * 4);
        assert_eq!(first.len() + second.len(), 48 * 4);
    }

    #[test]
    fn refused() {
        assert_eq!(segments(0, 0, 960, 4), None, "empty write");
        assert_eq!(segments(0, 961, 960, 4), None, "more than the ring");
        assert_eq!(segments(0, 1, 0, 4), None, "no ring");
        assert_eq!(segments(0, 1, 960, 0), None, "no frame size");
    }

    #[test]
    fn every_split_covers_the_write_once() {
        for ring in [1, 2, 3, 480, 960] {
            for from in 0..u64::from(ring) * 2 {
                for frames in 1..=ring {
                    let [first, second] = segments(from, frames, ring, 6).unwrap();
                    assert_eq!(first.len() + second.len(), frames as usize * 6);
                    assert!(first.end <= ring as usize * 6);
                    assert!(second.end <= first.start || second.is_empty());
                }
            }
        }
    }
}
