//! What the reader looks for in the recording, without system calls: crackles in a sine,
//! click edges, and the latencies' summary. The criteria are Résonnance's (`tag_micro`,
//! `tenue_cable`), so that the figures compare.

/// Counts the crackles of a recorded sine: a sample that the two before it do not predict
/// (a pure sine obeys `y[n] = c·y[n-1] - y[n-2]` with `c = 2 cos ω`), or a silence of 2 ms or
/// more. Counting starts after a warm-up, and two crackles are at least 100 ms apart, so that
/// one rupture counts once.
#[derive(Debug)]
pub struct Crackles {
    c: f32,
    y1: f32,
    y2: f32,
    samples: u64,
    last: u64,
    quiet: u64,
    warm_up: u64,
    apart: u64,
    silence: u64,
    /// Crackles counted.
    pub count: u64,
    /// Of which silences: a silence that began without breaking the prediction (an abrupt
    /// stop breaks it and counts as a crackle only).
    pub silences: u64,
}

impl Crackles {
    /// The largest deviation from the prediction a clean sine shows (rounding, the engine's
    /// float path).
    const TOLERANCE: f32 = 0.005;
    /// Below this amplitude a sample is silent.
    const QUIET: f32 = 0.01;

    /// A detector for a sine of `frequency` Hz recorded at `rate` Hz.
    #[must_use]
    pub fn new(frequency: f64, rate: u32) -> Self {
        let rate_u64 = u64::from(rate);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "2 cos ω is within [-2, 2]: an f32 holds it"
        )]
        let c = (2.0 * (core::f64::consts::TAU * frequency / f64::from(rate.max(1))).cos()) as f32;
        Self {
            c,
            y1: 0.0,
            y2: 0.0,
            samples: 0,
            last: 0,
            quiet: 0,
            warm_up: 2 * rate_u64,
            apart: rate_u64 / 10,
            silence: rate_u64 / 500,
            count: 0,
            silences: 0,
        }
    }

    /// One recorded sample.
    pub fn push(&mut self, y: f32) {
        self.quiet = if y.abs() < Self::QUIET {
            self.quiet + 1
        } else {
            0
        };
        let silent = self.quiet == self.silence;
        let broken = (y - self.c * self.y1 + self.y2).abs() > Self::TOLERANCE;
        if self.samples > self.warm_up
            && (broken || silent)
            && self.samples - self.last > self.apart
        {
            self.count += 1;
            if silent {
                self.silences += 1;
            }
            self.last = self.samples;
        }
        (self.y2, self.y1) = (self.y1, y);
        self.samples += 1;
    }
}

/// Finds a click's rising edge: a sample above the threshold after one below it.
#[derive(Debug, Default)]
pub struct Edges {
    previous: f32,
}

impl Edges {
    /// The click's amplitude is 0.8; the edge is where the signal crosses half of it.
    const THRESHOLD: f32 = 0.4;

    /// One recorded sample; whether it starts a click.
    pub fn push(&mut self, y: f32) -> bool {
        let rising = y.abs() > Self::THRESHOLD && self.previous.abs() <= Self::THRESHOLD;
        self.previous = y;
        rising
    }
}

/// The median, 95th percentile and maximum of `values` (sorted in place), or `None` when
/// there are none. The median is `tag_micro`'s (`values[len / 2]`, the upper middle of an even count),
/// so that medians compare; the 95th percentile by nearest rank.
#[must_use]
pub fn summary(values: &mut [f64]) -> Option<(f64, f64, f64)> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let rank = |percent: usize| values[(values.len() * percent).div_ceil(100).max(1) - 1];
    Some((values[values.len() / 2], rank(95), values[values.len() - 1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(n: u64, frequency: f64, rate: u32) -> f32 {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            reason = "test samples"
        )]
        let y =
            (0.5 * (core::f64::consts::TAU * frequency * n as f64 / f64::from(rate)).sin()) as f32;
        y
    }

    #[test]
    fn a_clean_sine_has_no_crackle() {
        let mut crackles = Crackles::new(440.0, 48_000);
        for n in 0..48_000 * 5 {
            crackles.push(sine(n, 440.0, 48_000));
        }
        assert_eq!(crackles.count, 0);
    }

    #[test]
    fn a_jump_and_a_silence_count_once_each() {
        let mut crackles = Crackles::new(440.0, 48_000);
        let mut n = 0u64;
        for _ in 0..48_000 * 3 {
            crackles.push(sine(n, 440.0, 48_000));
            n += 1;
        }
        // A jump: 100 frames lost.
        n += 100;
        for _ in 0..48_000 {
            crackles.push(sine(n, 440.0, 48_000));
            n += 1;
        }
        assert_eq!((crackles.count, crackles.silences), (1, 0));
        // 5 ms of silence, then the sine again: the abrupt stop breaks the prediction, so it
        // counts as one more crackle (the restart is within 100 ms of it).
        for _ in 0..240 {
            crackles.push(0.0);
        }
        for _ in 0..48_000 {
            crackles.push(sine(n, 440.0, 48_000));
            n += 1;
        }
        assert_eq!((crackles.count, crackles.silences), (2, 0));
    }

    #[test]
    fn nothing_counts_during_the_warm_up() {
        let mut crackles = Crackles::new(440.0, 48_000);
        for _ in 0..48_000 {
            crackles.push(0.0);
        }
        assert_eq!(crackles.count, 0);
    }

    #[test]
    fn click_edges() {
        let mut edges = Edges::default();
        let signal = [0.0, 0.0, 0.8, 0.8, 0.8, 0.0, 0.0, 0.8, 0.0];
        let found: Vec<bool> = signal.iter().map(|&y| edges.push(y)).collect();
        assert_eq!(
            found,
            [false, false, true, false, false, false, false, true, false]
        );
    }

    #[test]
    fn summaries() {
        assert_eq!(summary(&mut []), None);
        let mut one = [4.0];
        assert_eq!(summary(&mut one), Some((4.0, 4.0, 4.0)));
        let mut values: Vec<f64> = (1..=100).rev().map(f64::from).collect();
        assert_eq!(summary(&mut values), Some((51.0, 95.0, 100.0)));
    }
}
