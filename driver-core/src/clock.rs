//! Conversions between the performance counter (`KeQueryPerformanceCounter`) and audio frames.
//!
//! A stream's position is a pure function of the time elapsed since it entered the RUN state:
//! no carried remainder, no running sum, hence no state shared between the timer callback and
//! the position queries, and no drift. The intermediate products are 128-bit, so any 64-bit
//! tick count converts exactly (rounded down).

/// Frames elapsed after `ticks` performance counter ticks at `frequency` ticks per second, at
/// `rate` frames per second, rounded down.
///
/// Returns 0 when `frequency` is 0 (never the case for the performance counter) and saturates
/// at `u64::MAX`, so that no input can panic.
#[must_use]
pub fn frames_elapsed(ticks: u64, frequency: u64, rate: u32) -> u64 {
    if frequency == 0 {
        return 0;
    }
    let frames = u128::from(ticks) * u128::from(rate) / u128::from(frequency);
    u64::try_from(frames).unwrap_or(u64::MAX)
}

/// Performance counter ticks from the start of the stream to the sampling instant of frame
/// `frames`, rounded down: the inverse of [`frames_elapsed`].
///
/// Returns 0 when `rate` is 0 and saturates at `u64::MAX`.
#[must_use]
pub fn ticks_for_frames(frames: u64, frequency: u64, rate: u32) -> u64 {
    if rate == 0 {
        return 0;
    }
    let ticks = u128::from(frames) * u128::from(frequency) / u128::from(rate);
    u64::try_from(ticks).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The usual performance counter frequency on current Windows.
    const QPC: u64 = 10_000_000;

    #[test]
    fn one_second_is_one_rate() {
        assert_eq!(frames_elapsed(QPC, QPC, 48_000), 48_000);
        assert_eq!(ticks_for_frames(48_000, QPC, 48_000), QPC);
    }

    #[test]
    fn rounds_down() {
        // One frame lasts 208.33 ticks.
        assert_eq!(frames_elapsed(208, QPC, 48_000), 0);
        assert_eq!(frames_elapsed(209, QPC, 48_000), 1);
        assert_eq!(ticks_for_frames(1, QPC, 48_000), 208);
    }

    #[test]
    fn round_trip_never_moves_forward() {
        for frames in [0, 1, 47, 480, 48_000, 1 << 40] {
            let ticks = ticks_for_frames(frames, QPC, 48_000);
            let back = frames_elapsed(ticks, QPC, 48_000);
            assert!(back <= frames && frames - back <= 1, "{frames} -> {back}");
        }
    }

    #[test]
    fn no_overflow_after_years() {
        // Ten years of ticks: the product overflows 64 bits, not 128.
        let ticks = QPC * 3600 * 24 * 365 * 10;
        assert_eq!(
            frames_elapsed(ticks, QPC, 48_000),
            48_000 * 3600 * 24 * 365 * 10
        );
        assert_eq!(frames_elapsed(u64::MAX, 1, 48_000), u64::MAX);
        assert_eq!(ticks_for_frames(u64::MAX, u64::MAX, 1), u64::MAX);
    }

    #[test]
    fn zero_divisors_do_not_panic() {
        assert_eq!(frames_elapsed(QPC, 0, 48_000), 0);
        assert_eq!(ticks_for_frames(480, QPC, 0), 0);
    }
}
