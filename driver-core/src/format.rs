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

/// `KSAUDIO_SPEAKER_STEREO`: front left and front right.
pub const CHANNEL_MASK_STEREO: u32 = 0x3;

/// A GUID as its 16 bytes in memory (`Data1`, `Data2`, `Data3` little-endian, then `Data4`).
type GuidBytes = [u8; 16];

const fn guid(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> GuidBytes {
    let a = data1.to_le_bytes();
    let b = data2.to_le_bytes();
    let c = data3.to_le_bytes();
    [
        a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], data4[0], data4[1], data4[2], data4[3],
        data4[4], data4[5], data4[6], data4[7],
    ]
}

/// `KSDATAFORMAT_TYPE_AUDIO`.
const TYPE_AUDIO: GuidBytes = guid(
    0x7364_7561,
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71],
);
/// `KSDATAFORMAT_SUBTYPE_PCM`.
const SUBTYPE_PCM: GuidBytes = guid(
    0x0000_0001,
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71],
);
/// `KSDATAFORMAT_SPECIFIER_WAVEFORMATEX`.
const SPECIFIER_WAVEFORMATEX: GuidBytes = guid(
    0x0558_9f81,
    0xc356,
    0x11ce,
    [0xbf, 0x01, 0x00, 0xaa, 0x00, 0x55, 0x59, 0x5a],
);

/// `WAVE_FORMAT_PCM`.
const WAVE_FORMAT_PCM: u16 = 1;
/// `WAVE_FORMAT_EXTENSIBLE`.
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// Size of `KSDATAFORMAT`: four `ULONG`s and three GUIDs.
pub const KSDATAFORMAT_BYTES: usize = 16 + 3 * 16;
/// Size of `WAVEFORMATEX` (packed).
pub const WAVEFORMATEX_BYTES: usize = 18;
/// Bytes that `WAVEFORMATEXTENSIBLE` adds to `WAVEFORMATEX` (its `cbSize`).
pub const EXTENSIBLE_EXTRA_BYTES: usize = 22;

/// Whether `data`, a `KSDATAFORMAT` followed by its `WAVEFORMATEX` or `WAVEFORMATEXTENSIBLE`
/// (`KSDATAFORMAT_WAVEFORMATEX` or `KSDATAFORMAT_WAVEFORMATEXTENSIBLE`), describes the one
/// format the driver streams: 48 kHz, 16-bit PCM, stereo.
///
/// `data` is everything the caller knows to be readable; the format's own `FormatSize` must fit
/// in it. Attributes that may follow the format (`KSDATAFORMAT_ATTRIBUTES`, such as a signal
/// processing mode) are ignored: the driver has a single, default mode. No input can panic.
#[must_use]
pub fn is_supported(data: &[u8]) -> bool {
    is_format(data, false)
}

/// The answer to `KSPROPERTY_PIN_PROPOSEDATAFORMAT`: [`is_supported`], and also a bare
/// `KSDATAFORMAT` (no `WAVEFORMATEX`) asking for PCM audio.
///
/// Windows' audio endpoint builder probes a capture pin with bare `KSDATAFORMAT`s, one per
/// subtype (PCM, then compressed pass-through formats). Rejecting the PCM probe tells it the
/// pin cannot stream PCM at all, and the endpoint stays "not present" (seen in the M3 VM test).
#[must_use]
pub fn is_proposal_supported(data: &[u8]) -> bool {
    is_format(data, true)
}

fn is_format(data: &[u8], bare_pcm_allowed: bool) -> bool {
    let Some(format_size) = read_u32(data, 0) else {
        return false;
    };
    let Ok(format_size) = usize::try_from(format_size) else {
        return false;
    };
    if format_size < KSDATAFORMAT_BYTES || format_size > data.len() {
        return false;
    }
    let data = &data[..format_size];
    if guid_at(data, 16) != Some(TYPE_AUDIO)
        || guid_at(data, 32) != Some(SUBTYPE_PCM)
        || guid_at(data, 48) != Some(SPECIFIER_WAVEFORMATEX)
    {
        return false;
    }
    if format_size < KSDATAFORMAT_BYTES + WAVEFORMATEX_BYTES {
        return bare_pcm_allowed && format_size == KSDATAFORMAT_BYTES;
    }

    let wave = KSDATAFORMAT_BYTES;
    let fields = (
        read_u16(data, wave),
        read_u16(data, wave + 2),
        read_u32(data, wave + 4),
        read_u32(data, wave + 8),
        read_u16(data, wave + 12),
        read_u16(data, wave + 14),
    );
    let (
        Some(tag),
        Some(channels),
        Some(rate),
        Some(bytes_per_second),
        Some(block_align),
        Some(bits),
    ) = fields
    else {
        return false;
    };
    let common = channels == CHANNELS
        && rate == SAMPLE_RATE
        && bits == BITS_PER_SAMPLE
        && usize::from(block_align) == FRAME_BYTES
        && bytes_per_second == BYTES_PER_SECOND;
    if !common {
        return false;
    }
    match tag {
        WAVE_FORMAT_PCM => true,
        WAVE_FORMAT_EXTENSIBLE => {
            let extensible = wave + WAVEFORMATEX_BYTES;
            let extra = read_u16(data, wave + 16).map(usize::from);
            extra.is_some_and(|extra| extra >= EXTENSIBLE_EXTRA_BYTES)
                && read_u16(data, extensible) == Some(BITS_PER_SAMPLE)
                && read_u32(data, extensible + 2)
                    .is_some_and(|mask| mask == CHANNEL_MASK_STEREO || mask == 0)
                && guid_at(data, extensible + 6) == Some(SUBTYPE_PCM)
        }
        _ => false,
    }
}

fn read_u16(data: &[u8], at: usize) -> Option<u16> {
    let bytes = data.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(data: &[u8], at: usize) -> Option<u32> {
    let bytes = data.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn guid_at(data: &[u8], at: usize) -> Option<GuidBytes> {
    data.get(at..at.checked_add(16)?)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec::Vec;

    use super::*;

    /// `KSDATAFORMAT_WAVEFORMATEX` or, with `extensible`, `KSDATAFORMAT_WAVEFORMATEXTENSIBLE`,
    /// for 48 kHz, 16-bit, stereo PCM.
    fn format(extensible: bool) -> Vec<u8> {
        let size = KSDATAFORMAT_BYTES
            + WAVEFORMATEX_BYTES
            + if extensible {
                EXTENSIBLE_EXTRA_BYTES
            } else {
                0
            };
        let mut v = Vec::new();
        v.extend_from_slice(&u32::try_from(size).unwrap().to_le_bytes());
        v.extend_from_slice(&[0; 12]);
        v.extend_from_slice(&TYPE_AUDIO);
        v.extend_from_slice(&SUBTYPE_PCM);
        v.extend_from_slice(&SPECIFIER_WAVEFORMATEX);
        let tag = if extensible {
            WAVE_FORMAT_EXTENSIBLE
        } else {
            WAVE_FORMAT_PCM
        };
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&48_000u32.to_le_bytes());
        v.extend_from_slice(&192_000u32.to_le_bytes());
        v.extend_from_slice(&4u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        let extra = if extensible { 22u16 } else { 0 };
        v.extend_from_slice(&extra.to_le_bytes());
        if extensible {
            v.extend_from_slice(&16u16.to_le_bytes());
            v.extend_from_slice(&3u32.to_le_bytes());
            v.extend_from_slice(&SUBTYPE_PCM);
        }
        assert_eq!(v.len(), size);
        v
    }

    #[test]
    fn guid_bytes_follow_the_memory_layout() {
        // KSDATAFORMAT_TYPE_AUDIO is 73647561-0000-0010-8000-00aa00389b71 ("auds").
        assert_eq!(&TYPE_AUDIO[..4], b"auds");
    }

    #[test]
    fn accepts_the_format() {
        assert!(is_supported(&format(false)));
        assert!(is_supported(&format(true)));
    }

    #[test]
    fn ignores_trailing_attributes() {
        let mut data = format(true);
        data.extend_from_slice(&[0xAB; 40]);
        assert!(is_supported(&data));
    }

    #[test]
    fn rejects_other_formats() {
        let cases: [(usize, &[u8]); 7] = [
            (KSDATAFORMAT_BYTES + 2, &1u16.to_le_bytes()), // mono
            (KSDATAFORMAT_BYTES + 4, &44_100u32.to_le_bytes()), // 44.1 kHz
            (KSDATAFORMAT_BYTES + 14, &24u16.to_le_bytes()), // 24-bit
            (KSDATAFORMAT_BYTES + 12, &8u16.to_le_bytes()), // block align
            (KSDATAFORMAT_BYTES + 8, &1u32.to_le_bytes()), // bytes per second
            (KSDATAFORMAT_BYTES, &3u16.to_le_bytes()),     // IEEE float tag
            (32, &[0; 16]),                                // subtype
        ];
        for (at, bytes) in cases {
            let mut data = format(false);
            data[at..at + bytes.len()].copy_from_slice(bytes);
            assert!(!is_supported(&data), "patched at {at}");
        }
        let mut data = format(true);
        let sub_format = KSDATAFORMAT_BYTES + WAVEFORMATEX_BYTES + 6;
        data[sub_format] = 3; // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        assert!(!is_supported(&data));
    }

    #[test]
    fn bare_probes() {
        // What the endpoint builder sends: a KSDATAFORMAT alone, FormatSize 64.
        let mut probe = format(false);
        probe.truncate(KSDATAFORMAT_BYTES);
        probe[..4].copy_from_slice(&64u32.to_le_bytes());
        assert!(is_proposal_supported(&probe));
        // Never enough to open a stream.
        assert!(!is_supported(&probe));
        // Compressed pass-through probes (WAVE_FORMAT_DOLBY_AC3_SPDIF, WMA pro over S/PDIF).
        for subtype in [0x0092u32, 0x0164] {
            let mut other = probe.clone();
            other[32..36].copy_from_slice(&subtype.to_le_bytes());
            assert!(!is_proposal_supported(&other), "subtype {subtype:#x}");
        }
        // A size between the header and a full WAVEFORMATEX is neither.
        let mut odd = format(false);
        odd[..4].copy_from_slice(&70u32.to_le_bytes());
        assert!(!is_proposal_supported(&odd));
        // Full formats answer the same way.
        assert!(is_proposal_supported(&format(true)));
    }

    #[test]
    fn rejects_truncated_and_lying_sizes() {
        let data = format(true);
        for len in 0..data.len() {
            assert!(!is_supported(&data[..len]), "truncated to {len}");
        }
        // A FormatSize larger than the buffer.
        let mut data = format(false);
        data[0] = 0xFF;
        assert!(!is_supported(&data));
        // An extensible format whose FormatSize leaves out the extension.
        let mut data = format(true);
        let short = u32::try_from(KSDATAFORMAT_BYTES + WAVEFORMATEX_BYTES).unwrap();
        data[..4].copy_from_slice(&short.to_le_bytes());
        assert!(!is_supported(&data));
    }
}
