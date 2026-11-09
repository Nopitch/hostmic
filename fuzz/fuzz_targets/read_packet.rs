//! `GetReadPacket` at any position, packet size, buffer size and last read packet: never a
//! panic, and a returned packet is complete, intact in the buffer, after the last one read,
//! with exactly the overwritten packets counted as skipped.

#![no_main]

use hostmic_driver_core::packet::{self, completed_between};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: (u64, u64, u64, Option<u64>, u64)| {
    let (position, frames_per_packet, buffer_frames, last_read, before) = input;
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

    let Some(read) = packet::next_read(position, frames_per_packet, buffer_frames, last_read)
    else {
        return;
    };
    let start = u128::from(read.packet) * u128::from(frames_per_packet);
    let end = start + u128::from(frames_per_packet);
    // Complete: all its frames are below the position.
    assert!(end <= u128::from(position));
    // Intact: its first frame is at most a buffer behind the position.
    assert!(start + u128::from(buffer_frames) >= u128::from(position));
    // After the last read one, skipping exactly the packets in between.
    let next = last_read.map_or(0, |last| u128::from(last) + 1);
    assert!(u128::from(read.packet) >= next);
    assert_eq!(u128::from(read.packet) - next, u128::from(read.skipped));
    // More data exactly when the following packet is complete too.
    assert_eq!(
        read.more,
        end + u128::from(frames_per_packet) <= u128::from(position)
    );
});
