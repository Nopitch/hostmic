//! Samples from `f32` to the stream's sample type, without system calls.

use hostmic_abi::SampleType;

/// Encodes `samples` (nominal range -1.0 to 1.0) as `sample_type`, little-endian, into the start
/// of `out`; returns the bytes written, or `None` when `out` is too small. Values beyond the
/// range are clamped to it, and NaN becomes 0: a stream sample has nowhere else to go.
/// Integer types scale by their largest positive value and round to nearest.
#[must_use]
pub fn encode(samples: &[f32], sample_type: SampleType, out: &mut [u8]) -> Option<usize> {
    let size = sample_type.bytes() as usize;
    let bytes = samples.len().checked_mul(size)?;
    let out = out.get_mut(..bytes)?;
    for (sample, slot) in samples.iter().zip(out.chunks_exact_mut(size)) {
        let x = if sample.is_nan() {
            0.0
        } else {
            f64::from(sample.clamp(-1.0, 1.0))
        };
        // In range by the clamp: the casts cannot saturate, and rounding stays within it.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "values clamped to the type's range before the cast"
        )]
        match sample_type {
            SampleType::Int16 => {
                slot.copy_from_slice(&((x * f64::from(i16::MAX)).round() as i16).to_le_bytes());
            }
            SampleType::Int24 => {
                let value = (x * f64::from((1 << 23) - 1)).round() as i32;
                slot.copy_from_slice(&value.to_le_bytes()[..3]);
            }
            SampleType::Int32 => {
                slot.copy_from_slice(&((x * f64::from(i32::MAX)).round() as i32).to_le_bytes());
            }
            SampleType::Float32 => slot.copy_from_slice(&(x as f32).to_le_bytes()),
        }
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int16() {
        let mut out = [0u8; 10];
        let written = encode(&[0.0, 1.0, -1.0, 0.5, 2.0], SampleType::Int16, &mut out);
        assert_eq!(written, Some(10));
        let values: Vec<i16> = out
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
            .collect();
        assert_eq!(values, [0, 32_767, -32_767, 16_384, 32_767]);
    }

    #[test]
    fn int24() {
        let mut out = [0u8; 9];
        assert_eq!(
            encode(&[1.0, -1.0, 0.0], SampleType::Int24, &mut out),
            Some(9)
        );
        assert_eq!(out[..3], [0xFF, 0xFF, 0x7F]);
        assert_eq!(out[3..6], [0x01, 0x00, 0x80]);
        assert_eq!(out[6..], [0, 0, 0]);
    }

    #[test]
    fn int32_and_float() {
        let mut out = [0u8; 8];
        assert_eq!(encode(&[1.0, -0.25], SampleType::Int32, &mut out), Some(8));
        assert_eq!(i32::from_le_bytes(out[..4].try_into().unwrap()), i32::MAX);
        let mut out = [0u8; 8];
        assert_eq!(
            encode(&[0.25, -3.0], SampleType::Float32, &mut out),
            Some(8)
        );
        // Exact values: compared bit for bit.
        assert_eq!(out[..4], 0.25f32.to_le_bytes());
        assert_eq!(out[4..], (-1.0f32).to_le_bytes());
    }

    #[test]
    fn nan_and_room() {
        let mut out = [0xAAu8; 4];
        assert_eq!(
            encode(&[f32::NAN, f32::INFINITY], SampleType::Int16, &mut out),
            Some(4)
        );
        assert_eq!(out, [0, 0, 0xFF, 0x7F]);
        let mut small = [0u8; 3];
        assert_eq!(encode(&[0.0, 0.0], SampleType::Int16, &mut small), None);
        assert_eq!(encode(&[], SampleType::Int24, &mut small), Some(0));
    }
}
