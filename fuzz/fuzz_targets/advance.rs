//! `ADVANCE` inputs against any stream word and buffer size: never a panic, and an accepted
//! request moves a running stream's position forward by exactly the frames it names, within
//! the buffer and the position range, under the same generation.

#![no_main]

use hostmic_abi::AdvanceRequest;
use hostmic_driver_core::host::{self, Cursor, POSITION_MAX};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((head, input)) = data.split_first_chunk::<12>() else {
        return;
    };
    let (word, frames) = head.split_at(8);
    let word = u64::from_le_bytes(word.try_into().expect("8 bytes"));
    let buffer_frames = u32::from_le_bytes(frames.try_into().expect("4 bytes"));

    let Ok(new) = host::advance(input, word, buffer_frames) else {
        return;
    };
    let request = AdvanceRequest::from_bytes(input.try_into().expect("accepted input size"));
    let (before, after) = (Cursor::unpack(word), Cursor::unpack(new));
    assert!(before.is_running());
    assert_eq!(after.generation, before.generation);
    assert_eq!(request.generation, u32::from(before.generation));
    assert_eq!(request.from, before.position);
    assert!(request.frames != 0 && request.frames <= buffer_frames);
    assert_eq!(after.position, before.position + u64::from(request.frames));
    assert!(after.position <= POSITION_MAX);
});
