//! The test tone of milestone M3: a 1 kHz sine at half full scale (-6 dBFS), the same on every
//! channel.
//!
//! At 48 kHz one period of 1 kHz is exactly 48 frames, so the whole tone is a table of 48
//! samples computed at compile time, and frame `n` of the stream is `TABLE[n % 48]`. The phase
//! is continuous by construction, whatever the pauses, restarts and wrap-arounds of the ring
//! buffer, and the driver does no floating point at run time (Microsoft's sample computes a
//! `sin` per frame, between `KeSaveFloatingPointState` and `KeRestoreFloatingPointState`).

use crate::format::SAMPLE_RATE;

/// Frequency of the tone, in hertz.
pub const FREQUENCY: u32 = 1_000;

/// Frames per period of the tone.
pub const PERIOD_FRAMES: usize = {
    assert!(
        SAMPLE_RATE.is_multiple_of(FREQUENCY),
        "the period must be a whole number of frames"
    );
    (SAMPLE_RATE / FREQUENCY) as usize
};

/// Peak amplitude, as a fraction of full scale: half, that is -6 dBFS.
pub const AMPLITUDE: f64 = 0.5;

/// One period of the tone, as 16-bit samples.
pub static TABLE: [i16; PERIOD_FRAMES] = table();

/// The sample of frame `frame` of the stream (frame 0 is the start of the stream).
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    reason = "the remainder is below PERIOD_FRAMES, which is a usize"
)]
pub const fn sample(frame: u64) -> i16 {
    TABLE[(frame % PERIOD_FRAMES as u64) as usize]
}

const PI: f64 = core::f64::consts::PI;

#[allow(
    clippy::cast_precision_loss,
    reason = "the indices are below 48, exact in an f64"
)]
const fn table() -> [i16; PERIOD_FRAMES] {
    let mut table = [0; PERIOD_FRAMES];
    let mut i = 0;
    while i < PERIOD_FRAMES {
        let theta = 2.0 * PI * i as f64 / PERIOD_FRAMES as f64;
        table[i] = round_to_i16(AMPLITUDE * i16::MAX as f64 * sin(theta));
        i += 1;
    }
    table
}

/// `sin(x)` for `x` in `[0, 2π)`, in a `const fn` (`f64::sin` is not one).
///
/// Taylor series around 0 after reducing `x` to `[-π, π]`: with 13 terms the first omitted one
/// is below `π^27 / 27! ≈ 2·10^-15`, far below the 16-bit quantization step (`3·10^-5`).
#[allow(
    clippy::cast_precision_loss,
    reason = "the denominators are below 27 * 26, exact in an f64"
)]
const fn sin(x: f64) -> f64 {
    let x = if x > PI { x - 2.0 * PI } else { x };
    let mut term = x;
    let mut sum = x;
    let mut k = 1;
    while k < 13 {
        term = -term * x * x / ((2 * k) * (2 * k + 1)) as f64;
        sum += term;
        k += 1;
    }
    sum
}

/// Rounds half away from zero; the value must be within the `i16` range.
#[allow(
    clippy::cast_possible_truncation,
    reason = "the callers pass values within the i16 range"
)]
const fn round_to_i16(value: f64) -> i16 {
    if value >= 0.0 {
        (value + 0.5) as i16
    } else {
        (value - 0.5) as i16
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    #[test]
    fn period_is_48_frames() {
        assert_eq!(PERIOD_FRAMES, 48);
    }

    #[test]
    fn table_matches_std_sin() {
        for (i, &value) in TABLE.iter().enumerate() {
            let theta = 2.0 * PI * f64::from(u32::try_from(i).unwrap()) / 48.0;
            let expected = (AMPLITUDE * f64::from(i16::MAX) * theta.sin()).round();
            assert!(
                (f64::from(value) - expected).abs() <= 1.0,
                "frame {i}: {value}, expected {expected}"
            );
        }
    }

    #[test]
    fn peak_is_minus_6_dbfs() {
        let max = TABLE.iter().copied().max().unwrap();
        let min = TABLE.iter().copied().min().unwrap();
        // sin(2π·12/48) = 1: the peak falls exactly on frame 12.
        assert_eq!(max, 16_384);
        assert_eq!(min, -16_384);
        assert_eq!(TABLE[12], 16_384);
        assert_eq!(TABLE[36], -16_384);
    }

    #[test]
    fn no_dc_offset() {
        let sum: i32 = TABLE.iter().map(|&s| i32::from(s)).sum();
        assert_eq!(sum, 0);
    }

    #[test]
    fn sample_wraps_on_the_period() {
        assert_eq!(sample(0), 0);
        assert_eq!(sample(12), TABLE[12]);
        assert_eq!(sample(48 + 12), TABLE[12]);
        assert_eq!(sample(u64::MAX), TABLE[(u64::MAX % 48) as usize]);
    }
}
