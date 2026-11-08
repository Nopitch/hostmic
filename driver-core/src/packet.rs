//! Packet numbering of event-driven capture streams (`IMiniportWaveRTInputStream`).
//!
//! In event-driven mode the ring buffer is split into equal packets (two, for the audio
//! engine). The driver signals the notification event each time a packet is complete, and the
//! engine asks for the number of the last complete packet with `GetReadPacket`.

/// The 0-based number of the last complete packet once `written` frames of the stream are in
/// the buffer, or `None` while the first packet is incomplete (or `frames_per_packet` is 0).
#[must_use]
pub const fn last_complete(written: u64, frames_per_packet: u64) -> Option<u64> {
    if frames_per_packet == 0 {
        return None;
    }
    (written / frames_per_packet).checked_sub(1)
}

/// The number of packets completed between `before` and `after` written frames: the driver
/// signals the notification event when it is not 0.
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

    #[test]
    fn first_packet() {
        assert_eq!(last_complete(0, 480), None);
        assert_eq!(last_complete(479, 480), None);
        assert_eq!(last_complete(480, 480), Some(0));
        assert_eq!(last_complete(959, 480), Some(0));
        assert_eq!(last_complete(960, 480), Some(1));
    }

    #[test]
    fn boundaries() {
        assert_eq!(completed_between(0, 479, 480), 0);
        assert_eq!(completed_between(479, 480, 480), 1);
        assert_eq!(completed_between(480, 959, 480), 0);
        assert_eq!(completed_between(100, 1_500, 480), 3);
        assert_eq!(completed_between(500, 400, 480), 0);
    }

    #[test]
    fn zero_packet_size_does_not_panic() {
        assert_eq!(last_complete(1_000, 0), None);
        assert_eq!(completed_between(0, 1_000, 0), 0);
    }
}
