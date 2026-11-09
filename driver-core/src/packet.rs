//! Packet numbering of event-driven capture streams (`IMiniportWaveRTInputStream`).
//!
//! In event-driven mode the ring buffer is split into equal packets (two, for the audio
//! engine's shared mode). The host's `ADVANCE`s move the stream position; each packet they
//! complete signals the notification event, and the engine asks with `GetReadPacket` for the
//! next packet to read, until none is left.

/// What `GetReadPacket` returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Read {
    /// The packet's 0-based number since the stream started.
    pub packet: u64,
    /// Whether a later packet is complete too: the engine reads again at once.
    pub more: bool,
    /// Complete packets the engine never read, before this one, because the host overwrote
    /// them (it got more than a buffer ahead of the engine).
    pub skipped: u64,
}

/// The packet `GetReadPacket` returns at stream `position`, after the engine last read
/// `last_read` (`None`: nothing read yet), with packets of `frames_per_packet` frames in a
/// buffer of `buffer_frames` frames: the oldest complete packet not read yet whose frames are
/// all still in the buffer. `None` when there is none (or a size is 0, or the buffer is
/// smaller than a packet). No input can panic or overflow.
#[must_use]
pub const fn next_read(
    position: u64,
    frames_per_packet: u64,
    buffer_frames: u64,
    last_read: Option<u64>,
) -> Option<Read> {
    if frames_per_packet == 0 || buffer_frames < frames_per_packet {
        return None;
    }
    // Packets `0..complete` are complete.
    let complete = position / frames_per_packet;
    let mut next = match last_read {
        Some(packet) => match packet.checked_add(1) {
            Some(next) => next,
            None => return None,
        },
        None => 0,
    };
    // A packet is intact while its first frame is at most a buffer behind the position: the
    // host writes frame `n` where frame `n - buffer_frames` was.
    let oldest = position
        .saturating_sub(buffer_frames)
        .div_ceil(frames_per_packet);
    let mut skipped = 0;
    if next < oldest {
        skipped = oldest - next;
        next = oldest;
    }
    if next >= complete {
        return None;
    }
    Some(Read {
        packet: next,
        more: next + 1 < complete,
        skipped,
    })
}

/// The number of packets completed between stream positions `before` and `after`: the
/// driver signals the notification event when it is not 0.
#[must_use]
pub const fn completed_between(before: u64, after: u64, frames_per_packet: u64) -> u64 {
    if frames_per_packet == 0 || after <= before {
        return 0;
    }
    after / frames_per_packet - before / frames_per_packet
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The audio engine's shared mode: two packets of 10 ms at 48 kHz.
    const PACKET: u64 = 480;
    const BUFFER: u64 = 960;

    #[allow(
        clippy::unnecessary_wraps,
        reason = "compared with what `next_read` returns"
    )]
    fn read(packet: u64, more: bool, skipped: u64) -> Option<Read> {
        Some(Read {
            packet,
            more,
            skipped,
        })
    }

    #[test]
    fn first_packet() {
        assert_eq!(next_read(0, PACKET, BUFFER, None), None);
        assert_eq!(next_read(479, PACKET, BUFFER, None), None);
        assert_eq!(next_read(480, PACKET, BUFFER, None), read(0, false, 0));
        assert_eq!(next_read(959, PACKET, BUFFER, None), read(0, false, 0));
    }

    #[test]
    fn in_order_then_nothing() {
        assert_eq!(next_read(960, PACKET, BUFFER, None), read(0, true, 0));
        assert_eq!(next_read(960, PACKET, BUFFER, Some(0)), read(1, false, 0));
        assert_eq!(next_read(960, PACKET, BUFFER, Some(1)), None);
        assert_eq!(next_read(1_440, PACKET, BUFFER, Some(1)), read(2, false, 0));
    }

    #[test]
    fn overwritten_packets_are_skipped() {
        // Position 1200: frames 240.. are in the buffer, so packet 0 (frames 0..480) is
        // partly overwritten; packet 1 is the oldest intact one.
        assert_eq!(next_read(1_200, PACKET, BUFFER, None), read(1, false, 1));
        // Far ahead: packets 2..9 are gone, 10 and 11 complete and intact.
        assert_eq!(next_read(5_760, PACKET, BUFFER, Some(1)), read(10, true, 8));
        // Exactly one buffer ahead: nothing is overwritten yet.
        assert_eq!(next_read(1_440, PACKET, BUFFER, Some(0)), read(1, true, 0));
    }

    #[test]
    fn completed_between_boundaries() {
        assert_eq!(completed_between(0, 479, PACKET), 0);
        assert_eq!(completed_between(479, 480, PACKET), 1);
        assert_eq!(completed_between(480, 959, PACKET), 0);
        assert_eq!(completed_between(100, 1_500, PACKET), 3);
        assert_eq!(completed_between(500, 400, PACKET), 0);
    }

    #[test]
    fn degenerate_sizes_and_extremes_do_not_panic() {
        assert_eq!(next_read(1_000, 0, BUFFER, None), None);
        assert_eq!(next_read(1_000, PACKET, 0, None), None);
        assert_eq!(next_read(1_000, PACKET, PACKET - 1, None), None);
        assert_eq!(next_read(u64::MAX, PACKET, BUFFER, Some(u64::MAX)), None);
        assert_eq!(next_read(u64::MAX, 1, u64::MAX, None), read(0, true, 0));
        assert_eq!(completed_between(0, 1_000, 0), 0);
        for position in 0..5_000 {
            for last in [None, Some(0), Some(3), Some(9)] {
                if let Some(r) = next_read(position, PACKET, BUFFER, last) {
                    assert!((r.packet + 1) * PACKET <= position, "complete");
                    assert!(r.packet * PACKET + BUFFER >= position, "intact");
                    assert!(last.is_none_or(|l| r.packet > l), "after the last read");
                }
            }
        }
    }
}
