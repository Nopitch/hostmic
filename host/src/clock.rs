//! The reference loop's clock, without system calls: how many frames the performance counter
//! says are due.

/// Frames at `rate` per second in `ticks` performance counter ticks at `frequency` per second,
/// rounded down; 0 for a negative count or a zero frequency.
#[must_use]
pub fn frames_in(ticks: i64, rate: u32, frequency: i64) -> u64 {
    let (Ok(ticks), Ok(frequency)) = (u64::try_from(ticks), u64::try_from(frequency)) else {
        return 0;
    };
    if frequency == 0 {
        return 0;
    }
    let frames = u128::from(ticks) * u128::from(rate) / u128::from(frequency);
    u64::try_from(frames).unwrap_or(u64::MAX)
}

/// Performance counter ticks at `frequency` per second for `frames` frames at `rate` per
/// second, rounded down: the inverse of [`frames_in`].
#[must_use]
pub fn ticks_for(frames: u64, rate: u32, frequency: i64) -> i64 {
    let frequency = u128::from(u64::try_from(frequency).unwrap_or(0));
    let ticks = u128::from(frames) * frequency / u128::from(rate.max(1));
    i64::try_from(ticks).unwrap_or(i64::MAX)
}

/// How many requests `frames` frames go out in, for about `nominal` frames per request: each
/// part then holds between `nominal` and `2 × nominal - 1` frames (fewer only when `frames`
/// itself is fewer), never a remainder of a frame or two.
#[must_use]
pub fn parts(frames: u32, nominal: u32) -> u32 {
    (frames / nominal.max(1)).max(1)
}

/// The first frame of part `index` when `frames` frames are cut into `parts` parts; part
/// `index` ends where part `index + 1` starts, and `part_start(frames, parts, parts)` is
/// `frames`. Parts differ by at most one frame.
#[must_use]
pub fn part_start(frames: u32, parts: u32, index: u32) -> u32 {
    let start = u64::from(frames) * u64::from(index.min(parts)) / u64::from(parts.max(1));
    u32::try_from(start).unwrap_or(frames)
}

/// The bucket of a gap of `ticks` performance counter ticks at `frequency` per second, against
/// ascending bucket edges in microseconds: `0` below the first edge, `edges.len()` from the
/// last one on. Compared in ticks, without rounding.
#[must_use]
pub fn gap_bucket(ticks: u64, frequency: i64, edges_us: &[u32]) -> usize {
    let frequency = u128::from(u64::try_from(frequency).unwrap_or(0));
    // ticks / frequency >= edge / 10^6  <=>  ticks × 10^6 >= edge × frequency
    let scaled = u128::from(ticks) * 1_000_000;
    edges_us
        .iter()
        .take_while(|&&edge| scaled >= u128::from(edge) * frequency)
        .count()
}

/// Where the clock starts: a performance counter value and the stream position it stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    /// The performance counter at the anchor.
    pub qpc: i64,
    /// The stream position at the anchor.
    pub position: u64,
}

/// What the clock says to write now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Due {
    /// Nothing yet: the stream is not behind the clock.
    Nothing,
    /// These frames, from the current position.
    Frames(u32),
    /// More than `limit` frames are due: the host was not scheduled for more than a buffer, and
    /// the audio engine has lost those frames anyway. Write `limit` frames and anchor the clock
    /// again at the current counter value, the position after them.
    Late {
        /// The frames the clock wanted.
        wanted: u64,
    },
}

/// The frames due at counter value `now` for a stream at `position`, against `anchor`, at
/// `rate` frames and `frequency` ticks per second; a write never exceeds `limit` frames (the
/// stream's buffer).
#[must_use]
pub fn due(anchor: Anchor, now: i64, position: u64, rate: u32, frequency: i64, limit: u32) -> Due {
    let target =
        anchor
            .position
            .saturating_add(frames_in(now.saturating_sub(anchor.qpc), rate, frequency));
    if target <= position {
        return Due::Nothing;
    }
    let wanted = target - position;
    match u32::try_from(wanted) {
        Ok(frames) if frames <= limit => Due::Frames(frames),
        _ => Due::Late { wanted },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 10 MHz performance counter, as on most current systems.
    const QPC: i64 = 10_000_000;

    #[test]
    fn frames_in_ticks() {
        assert_eq!(frames_in(QPC, 48_000, QPC), 48_000);
        assert_eq!(frames_in(10_000, 48_000, QPC), 48, "1 ms");
        assert_eq!(frames_in(9_999, 48_000, QPC), 47, "rounded down");
        assert_eq!(frames_in(-1, 48_000, QPC), 0);
        assert_eq!(frames_in(1, 48_000, 0), 0);
        // Days of ticks do not overflow.
        assert_eq!(
            frames_in(QPC * 86_400 * 30, 48_000, QPC),
            48_000 * 86_400 * 30
        );
    }

    #[test]
    fn ticks_for_frames() {
        assert_eq!(ticks_for(48, 48_000, QPC), 10_000, "1 ms");
        assert_eq!(ticks_for(0, 48_000, QPC), 0);
        assert_eq!(ticks_for(1, 0, QPC), QPC, "a zero rate counts as 1");
        assert_eq!(ticks_for(1, 48_000, -1), 0);
        assert_eq!(ticks_for(u64::MAX, 1, QPC), i64::MAX, "saturates");
        assert_eq!(frames_in(ticks_for(480, 48_000, QPC), 48_000, QPC), 480);
    }

    #[test]
    fn requests() {
        // A late tick stays one request; whole milliseconds split evenly.
        assert_eq!(parts(49, 48), 1);
        assert_eq!(parts(47, 48), 1);
        assert_eq!(parts(96, 48), 2);
        assert_eq!(parts(384, 48), 8);
        assert_eq!(parts(960, 48), 20);
        assert_eq!(parts(5, 0), 5, "a zero nominal size counts as 1");
        for frames in 1..=2_000u32 {
            for nominal in [44, 48, 96] {
                let n = parts(frames, nominal);
                assert_eq!(part_start(frames, n, 0), 0);
                assert_eq!(part_start(frames, n, n), frames);
                for index in 0..n {
                    let size = part_start(frames, n, index + 1) - part_start(frames, n, index);
                    assert!(size >= 1);
                    assert!(frames < nominal || (nominal..2 * nominal).contains(&size));
                }
            }
        }
    }

    #[test]
    fn gap_buckets() {
        let edges = [1_000, 2_000, 5_000, 20_000];
        assert_eq!(gap_bucket(0, QPC, &edges), 0);
        assert_eq!(gap_bucket(9_999, QPC, &edges), 0, "just under 1 ms");
        assert_eq!(
            gap_bucket(10_000, QPC, &edges),
            1,
            "1 ms is the second bucket"
        );
        assert_eq!(gap_bucket(14_999, QPC, &[1_500]), 0, "just under 1.5 ms");
        assert_eq!(gap_bucket(15_000, QPC, &[1_500]), 1);
        assert_eq!(gap_bucket(49_999, QPC, &edges), 2);
        assert_eq!(gap_bucket(50_000, QPC, &edges), 3);
        assert_eq!(
            gap_bucket(200_000, QPC, &edges),
            4,
            "the last bucket is open"
        );
        assert_eq!(gap_bucket(u64::MAX, QPC, &edges), 4);
        assert_eq!(
            gap_bucket(5, 0, &edges),
            4,
            "a zero frequency puts every gap last"
        );
    }

    #[test]
    fn due_frames() {
        let anchor = Anchor {
            qpc: 1_000,
            position: 960,
        };
        assert_eq!(due(anchor, 1_000, 960, 48_000, QPC, 960), Due::Nothing);
        assert_eq!(
            due(anchor, 1_000 + 10_000, 960, 48_000, QPC, 960),
            Due::Frames(48)
        );
        // Already written past the clock (a write rounded up elsewhere): nothing.
        assert_eq!(
            due(anchor, 1_000 + 10_000, 1_100, 48_000, QPC, 960),
            Due::Nothing
        );
        // Part of it written.
        assert_eq!(
            due(anchor, 1_000 + 20_000, 1_008, 48_000, QPC, 960),
            Due::Frames(48)
        );
        // Exactly a buffer is still a write.
        assert_eq!(
            due(anchor, 1_000 + 200_000, 960, 48_000, QPC, 960),
            Due::Frames(960)
        );
    }

    #[test]
    fn late() {
        let anchor = Anchor {
            qpc: 0,
            position: 0,
        };
        // 30 ms late with a 20 ms buffer.
        assert_eq!(
            due(anchor, 300_000, 0, 48_000, QPC, 960),
            Due::Late { wanted: 1_440 }
        );
        // A counter that went backwards is no write.
        assert_eq!(due(anchor, -5, 0, 48_000, QPC, 960), Due::Nothing);
    }
}
