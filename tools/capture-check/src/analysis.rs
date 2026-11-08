//! Analysis of a recorded sine: frequency, level and discontinuities. Pure arithmetic, tested
//! on synthetic signals.

/// What a recording of a sine looks like.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// Frames analysed.
    pub frames: usize,
    /// Frequency estimated from the rising zero crossings, in hertz; `None` without at least two
    /// crossings.
    pub frequency: Option<f64>,
    /// Peak level, in dBFS (`-inf` for silence).
    pub peak_dbfs: f64,
    /// RMS level, in dBFS (`-inf` for silence).
    pub rms_dbfs: f64,
    /// Places where the signal departs from a sine of the measured frequency: dropped,
    /// repeated or zeroed samples, clicks. Neighbouring departures count once.
    pub glitches: usize,
    /// Frame index of the first glitch.
    pub first_glitch: Option<usize>,
}

/// A departure from the sine larger than this fraction of the peak counts as a glitch. A clean
/// 16-bit sine stays below 0.1 %; a single dropped sample at 1 kHz / 48 kHz moves the signal by
/// about 13 % of the peak.
const GLITCH_THRESHOLD: f64 = 0.01;

/// Analyses `samples`, one channel at `rate` frames per second.
#[must_use]
pub fn analyse(samples: &[f32], rate: u32) -> Report {
    let frames = samples.len();
    let peak = samples
        .iter()
        .map(|s| f64::from(s.abs()))
        .fold(0.0_f64, f64::max);
    let mean_square = if frames == 0 {
        0.0
    } else {
        samples
            .iter()
            .map(|&s| f64::from(s) * f64::from(s))
            .sum::<f64>()
            / to_f64(frames)
    };

    // Rising zero crossings, interpolated between samples.
    let crossings: Vec<f64> = samples
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w[0] < 0.0 && w[1] >= 0.0)
        .map(|(i, w)| {
            let (a, b) = (f64::from(w[0]), f64::from(w[1]));
            to_f64(i) + a / (a - b)
        })
        .collect();
    let frequency = match (crossings.first(), crossings.last()) {
        (Some(&first), Some(&last)) if crossings.len() >= 2 && last > first => {
            Some(to_f64(crossings.len() - 1) * f64::from(rate) / (last - first))
        }
        _ => None,
    };

    // A sine obeys x[n+1] = 2 cos(w) x[n] - x[n-1]; anything else is a glitch.
    let (glitches, first_glitch) = match frequency {
        Some(f) if peak > 0.0 => {
            let k = 2.0 * (2.0 * core::f64::consts::PI * f / f64::from(rate)).cos();
            let limit = GLITCH_THRESHOLD * peak;
            let mut count = 0;
            let mut first = None;
            let mut last_bad: Option<usize> = None;
            for (i, w) in samples.windows(3).enumerate() {
                let residual = f64::from(w[2]) - k * f64::from(w[1]) + f64::from(w[0]);
                if residual.abs() > limit {
                    if last_bad.is_none_or(|j| i > j + 2) {
                        count += 1;
                        first.get_or_insert(i + 1);
                    }
                    last_bad = Some(i);
                }
            }
            (count, first)
        }
        _ => (0, None),
    };

    Report {
        frames,
        frequency,
        peak_dbfs: 20.0 * peak.log10(),
        rms_dbfs: 10.0 * mean_square.log10(),
        glitches,
        first_glitch,
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "frame counts are far below 2^52"
)]
fn to_f64(n: usize) -> f64 {
    n as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::cast_possible_truncation, reason = "test signals")]
    fn sine(frequency: f64, amplitude: f64, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|n| {
                let t = to_f64(n) / 48_000.0;
                (amplitude * (2.0 * core::f64::consts::PI * frequency * t).sin()) as f32
            })
            .collect()
    }

    /// Rounds to 16-bit steps, as the driver's tone is.
    #[allow(clippy::cast_possible_truncation, reason = "test signals")]
    fn quantize(samples: &mut [f32]) {
        for s in samples {
            *s = ((f64::from(*s) * 32_767.0).round() / 32_767.0) as f32;
        }
    }

    #[test]
    fn clean_tone() {
        let mut samples = sine(1_000.0, 0.5, 48_000 * 5);
        quantize(&mut samples);
        let report = analyse(&samples, 48_000);
        let frequency = report.frequency.unwrap();
        assert!((frequency - 1_000.0).abs() < 0.01, "{frequency}");
        assert!(
            (report.peak_dbfs + 6.02).abs() < 0.01,
            "{}",
            report.peak_dbfs
        );
        assert!((report.rms_dbfs + 9.03).abs() < 0.01, "{}", report.rms_dbfs);
        assert_eq!(report.glitches, 0);
        assert_eq!(report.first_glitch, None);
    }

    #[test]
    fn dropped_samples() {
        let mut samples = sine(1_000.0, 0.5, 48_000);
        samples.drain(10_000..10_005);
        let report = analyse(&samples, 48_000);
        assert_eq!(report.glitches, 1);
        assert!(report.first_glitch.unwrap().abs_diff(10_000) <= 2);
    }

    #[test]
    fn a_zeroed_packet_counts_twice() {
        // A packet of silence: one glitch where it starts, one where the sine comes back.
        let mut samples = sine(1_000.0, 0.5, 48_000);
        samples[20_000..20_480].fill(0.0);
        let report = analyse(&samples, 48_000);
        assert_eq!(report.glitches, 2);
    }

    #[test]
    fn silence() {
        let report = analyse(&vec![0.0; 4_800], 48_000);
        assert_eq!(report.frequency, None);
        assert_eq!(report.glitches, 0);
        assert!(report.peak_dbfs.is_infinite());
    }

    #[test]
    fn empty() {
        let report = analyse(&[], 48_000);
        assert_eq!(report.frames, 0);
        assert_eq!(report.frequency, None);
    }
}
