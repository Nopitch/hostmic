//! `GetReadPacket` at any position, packet size, buffer size, last read packet and write-ahead:
//! never a panic; a returned packet is complete, intact in the buffer, after the last one read
//! and the oldest such packet, with exactly the overwritten packets counted as skipped; `None`
//! only when no packet qualifies.

#![no_main]

use hostmic_driver_core::packet::{self, completed_between};
use libfuzzer_sys::fuzz_target;

/// Whether packet `p` qualifies: complete, intact while `ahead` frames are being written, and
/// not read yet.
fn qualifies(
    p: u128,
    position: u128,
    per_packet: u128,
    buffer: u128,
    ahead: u128,
    next: u128,
) -> bool {
    (p + 1) * per_packet <= position && p * per_packet + buffer >= position + ahead && p >= next
}

fuzz_target!(|input: (u64, u64, u64, Option<u64>, u64, u64)| {
    let (position, frames_per_packet, buffer_frames, last_read, ahead, before) = input;
    // `completed_between` never panics and counts packet boundaries.
    let completed = completed_between(before, position, frames_per_packet);
    if frames_per_packet != 0 && position > before {
        assert_eq!(
            completed,
            position / frames_per_packet - before / frames_per_packet
        );
    } else {
        assert_eq!(completed, 0);
    }

    let read = packet::next_read(position, frames_per_packet, buffer_frames, last_read, ahead);
    if frames_per_packet == 0 || buffer_frames < frames_per_packet || last_read == Some(u64::MAX) {
        assert_eq!(read, None);
        return;
    }
    let (pos, per, buf) = (
        u128::from(position),
        u128::from(frames_per_packet),
        u128::from(buffer_frames),
    );
    // The frames written ahead count as written, up to the end of the stream's numbering.
    let ahead = u128::from(position.saturating_add(ahead)) - pos;
    let next = last_read.map_or(0, |last| u128::from(last) + 1);
    // The oldest qualifying packet, if any: the first intact one at or after `next`.
    let oldest_intact = (pos + ahead).saturating_sub(buf).div_ceil(per);
    let first = next.max(oldest_intact);
    let expected = qualifies(first, pos, per, buf, ahead, next).then_some(first);
    match read {
        None => assert_eq!(expected, None, "a qualifying packet was not returned"),
        Some(read) => {
            let p = u128::from(read.packet);
            assert_eq!(Some(p), expected, "the oldest qualifying packet");
            assert!(qualifies(p, pos, per, buf, ahead, next));
            assert_eq!(p - next, u128::from(read.skipped));
            // More data exactly when the following packet is complete too.
            assert_eq!(read.more, (p + 2) * per <= pos);
        }
    }
});
