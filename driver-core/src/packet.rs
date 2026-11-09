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

/// The last packet the engine read, and the stream run it belongs to, packed in one 64-bit
/// word so that one compare-and-exchange moves both: a stop starts a new `epoch`, and a
/// `GetReadPacket` that read the old run's position cannot store its packet into the new run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadMark {
    /// Incremented (wrapping) by each stop.
    pub epoch: u16,
    /// The last packet read in this run; `None` before the first.
    pub last: Option<u64>,
}

impl ReadMark {
    /// Packet numbers fit in 48 bits: positions do (`host::POSITION_MAX`).
    const PACKET_BITS: u32 = 48;
    const PACKET_MASK: u64 = (1 << Self::PACKET_BITS) - 1;

    /// The packed word: the epoch in the top 16 bits, then the last packet plus one (0: none).
    /// A packet number that does not fit (never the case for a stream position) packs as none.
    #[must_use]
    pub const fn pack(self) -> u64 {
        let last = match self.last {
            Some(packet) if packet < Self::PACKET_MASK => packet + 1,
            _ => 0,
        };
        ((self.epoch as u64) << Self::PACKET_BITS) | last
    }

    /// The inverse of [`ReadMark::pack`].
    #[must_use]
    pub const fn unpack(word: u64) -> Self {
        #[allow(clippy::cast_possible_truncation, reason = "the top 16 bits")]
        let epoch = (word >> Self::PACKET_BITS) as u16;
        let last = word & Self::PACKET_MASK;
        Self {
            epoch,
            last: if last == 0 { None } else { Some(last - 1) },
        }
    }

    /// The mark of the next run: nothing read yet.
    #[must_use]
    pub const fn stopped(self) -> Self {
        Self {
            epoch: self.epoch.wrapping_add(1),
            last: None,
        }
    }

    /// The same run, with `packet` read.
    #[must_use]
    pub const fn read(self, packet: u64) -> Self {
        Self {
            epoch: self.epoch,
            last: Some(packet),
        }
    }
}

/// The packet `GetReadPacket` returns at stream `position`, after the engine last read
/// `last_read` (`None`: nothing read yet), with packets of `frames_per_packet` frames in a
/// buffer of `buffer_frames` frames, when the host may already have written `ahead` frames
/// beyond the position (those of its next `ADVANCE`): the oldest complete packet not read yet
/// whose frames are all still in the buffer. `None` when there is none (or a size is 0, or
/// the buffer is smaller than a packet). No input can panic or overflow.
///
/// The host writes frame `n` where frame `n - buffer_frames` was, before it advances past it:
/// a packet is intact while its first frame is at least a buffer behind the frames the host
/// may be writing. The driver takes `ahead` to be the size of the host's last `ADVANCE`, an
/// estimate: a host that writes more at once than it usually advances (catching up after a
/// stall) can overwrite a packet this still calls intact.
#[must_use]
pub const fn next_read(
    position: u64,
    frames_per_packet: u64,
    buffer_frames: u64,
    last_read: Option<u64>,
    ahead: u64,
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
    let written = position.saturating_add(ahead);
    let oldest = written
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
        assert_eq!(next_read(0, PACKET, BUFFER, None, 0), None);
        assert_eq!(next_read(479, PACKET, BUFFER, None, 0), None);
        assert_eq!(next_read(480, PACKET, BUFFER, None, 0), read(0, false, 0));
        assert_eq!(next_read(959, PACKET, BUFFER, None, 0), read(0, false, 0));
    }

    #[test]
    fn in_order_then_nothing() {
        assert_eq!(next_read(960, PACKET, BUFFER, None, 0), read(0, true, 0));
        assert_eq!(
            next_read(960, PACKET, BUFFER, Some(0), 0),
            read(1, false, 0)
        );
        assert_eq!(next_read(960, PACKET, BUFFER, Some(1), 0), None);
        assert_eq!(
            next_read(1_440, PACKET, BUFFER, Some(1), 0),
            read(2, false, 0)
        );
    }

    #[test]
    fn overwritten_packets_are_skipped() {
        // Position 1200: frames 240.. are in the buffer, so packet 0 (frames 0..480) is
        // partly overwritten; packet 1 is the oldest intact one.
        assert_eq!(next_read(1_200, PACKET, BUFFER, None, 0), read(1, false, 1));
        // Far ahead: packets 2..9 are gone, 10 and 11 complete and intact.
        assert_eq!(
            next_read(5_760, PACKET, BUFFER, Some(1), 0),
            read(10, true, 8)
        );
        // Exactly one buffer ahead: nothing is overwritten yet.
        assert_eq!(
            next_read(1_440, PACKET, BUFFER, Some(0), 0),
            read(1, true, 0)
        );
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
        assert_eq!(next_read(1_000, 0, BUFFER, None, 0), None);
        assert_eq!(next_read(1_000, PACKET, 0, None, 0), None);
        assert_eq!(next_read(1_000, PACKET, PACKET - 1, None, 0), None);
        assert_eq!(next_read(u64::MAX, PACKET, BUFFER, Some(u64::MAX), 0), None);
        assert_eq!(next_read(u64::MAX, 1, u64::MAX, None, 0), read(0, true, 0));
        assert_eq!(completed_between(0, 1_000, 0), 0);
        assert_eq!(
            next_read(960, PACKET, BUFFER, None, BUFFER),
            None,
            "a whole buffer written ahead overwrites every complete packet"
        );
        let _ = next_read(u64::MAX, PACKET, BUFFER, Some(7), u64::MAX);
        for position in 0..5_000 {
            for last in [None, Some(0), Some(3), Some(9)] {
                for ahead in [0, 48, 479] {
                    if let Some(r) = next_read(position, PACKET, BUFFER, last, ahead) {
                        assert!((r.packet + 1) * PACKET <= position, "complete");
                        assert!(r.packet * PACKET + BUFFER >= position + ahead, "intact");
                        assert!(last.is_none_or(|l| r.packet > l), "after the last read");
                    }
                }
            }
        }
    }

    #[test]
    fn read_marks_round_trip_and_stops_change_the_epoch() {
        for mark in [
            ReadMark {
                epoch: 0,
                last: None,
            },
            ReadMark {
                epoch: 7,
                last: Some(0),
            },
            ReadMark {
                epoch: u16::MAX,
                last: Some((1 << 48) - 2),
            },
        ] {
            assert_eq!(ReadMark::unpack(mark.pack()), mark);
        }
        let mark = ReadMark {
            epoch: u16::MAX,
            last: Some(41),
        };
        assert_eq!(
            mark.stopped(),
            ReadMark {
                epoch: 0,
                last: None
            }
        );
        assert_ne!(mark.stopped().pack(), mark.pack());
        // A stop after nothing was read still changes the word: no ABA.
        let fresh = ReadMark {
            epoch: 3,
            last: None,
        };
        assert_ne!(fresh.stopped().pack(), fresh.pack());
        assert_eq!(fresh.read(5).last, Some(5));
        // Out of range packs as none instead of corrupting the epoch.
        assert_eq!(
            ReadMark::unpack(
                ReadMark {
                    epoch: 2,
                    last: Some(u64::MAX)
                }
                .pack()
            ),
            ReadMark {
                epoch: 2,
                last: None
            }
        );
    }

    /// The host writes the frames of its next `ADVANCE` before sending it: they may already
    /// overwrite the oldest packet.
    #[test]
    fn frames_written_ahead_count_as_overwriting() {
        // Position 960, host writing 960..1008 into packet 0's place: packet 0 is gone.
        assert_eq!(next_read(960, PACKET, BUFFER, None, 48), read(1, false, 1));
        // Nothing written ahead yet: packet 0 is still whole.
        assert_eq!(next_read(960, PACKET, BUFFER, None, 0), read(0, true, 0));
        // One millisecond into packet 2's writing, packet 1 is still whole.
        assert_eq!(
            next_read(1_008, PACKET, BUFFER, Some(0), 48),
            read(1, false, 0)
        );
    }
}
