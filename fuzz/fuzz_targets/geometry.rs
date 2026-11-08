//! Stream buffer requests from PortCls: never a panic, and an accepted geometry is at least
//! the requested size, within the capacity, in whole frames and whole packets.

#![no_main]

use hostmic_driver_core::buffer;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: (u32, u32, u32, u32)| {
    let (requested, packets, frame_bytes, capacity) = input;
    let Some(geometry) = buffer::geometry(requested, packets, frame_bytes, capacity) else {
        return;
    };
    assert!(geometry.bytes >= requested);
    assert!(geometry.bytes <= capacity);
    assert_eq!(
        u64::from(geometry.bytes),
        u64::from(geometry.frames) * u64::from(frame_bytes)
    );
    // Less than one frame of rounding in polling mode, less than one per packet otherwise.
    assert!(
        u64::from(geometry.frames) * u64::from(frame_bytes)
            < u64::from(requested) + u64::from(frame_bytes) * u64::from(packets.max(1))
    );
    if packets == 0 {
        assert_eq!(geometry.frames_per_packet, 0);
    } else {
        assert_eq!(
            u64::from(geometry.frames),
            u64::from(geometry.frames_per_packet) * u64::from(packets)
        );
    }
});
