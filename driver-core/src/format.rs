//! The one stream format of milestone M3: 48 kHz, 16-bit signed PCM, stereo, interleaved.
//!
//! It is the format the host library will offer first (PLAN.md, "Host library").

/// Frames per second.
pub const SAMPLE_RATE: u32 = 48_000;

/// Channels per frame.
pub const CHANNELS: u16 = 2;

/// Bits per sample.
pub const BITS_PER_SAMPLE: u16 = 16;

/// Bytes per sample.
pub const SAMPLE_BYTES: usize = BITS_PER_SAMPLE as usize / 8;

/// Bytes per frame (one sample per channel): `WAVEFORMATEX::nBlockAlign`.
pub const FRAME_BYTES: usize = CHANNELS as usize * SAMPLE_BYTES;

/// Bytes per second: `WAVEFORMATEX::nAvgBytesPerSec`.
#[allow(
    clippy::cast_possible_truncation,
    reason = "4 bytes per frame at 48 kHz fit a u32; checked at compile time"
)]
pub const BYTES_PER_SECOND: u32 = {
    let bytes = SAMPLE_RATE as usize * FRAME_BYTES;
    assert!(bytes <= u32::MAX as usize);
    bytes as u32
};
