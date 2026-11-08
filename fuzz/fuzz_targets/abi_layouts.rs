//! Every layout of the host interface decodes any bytes and encodes them back unchanged: the
//! driver's view of a request is exactly what the host sent, with no field lost or reordered.

#![no_main]

use hostmic_abi::{
    AdvanceReply, AdvanceRequest, AttachReply, AttachRequest, Format, FormatRange, Info,
    StreamState,
};
use libfuzzer_sys::fuzz_target;

/// Checks `decode` then `encode` on the first `N` bytes of `data`, when there are enough.
fn round_trip<const N: usize, T>(
    data: &[u8],
    decode: fn(&[u8; N]) -> T,
    encode: fn(&T) -> [u8; N],
) {
    if let Some(bytes) = data.first_chunk::<N>() {
        assert_eq!(&encode(&decode(bytes)), bytes);
    }
}

fuzz_target!(|data: &[u8]| {
    round_trip(data, Format::from_bytes, Format::to_bytes);
    round_trip(data, FormatRange::from_bytes, FormatRange::to_bytes);
    round_trip(data, Info::from_bytes, Info::to_bytes);
    round_trip(data, AttachRequest::from_bytes, AttachRequest::to_bytes);
    round_trip(data, AttachReply::from_bytes, AttachReply::to_bytes);
    round_trip(data, StreamState::from_bytes, StreamState::to_bytes);
    round_trip(data, AdvanceRequest::from_bytes, AdvanceRequest::to_bytes);
    round_trip(data, AdvanceReply::from_bytes, AdvanceReply::to_bytes);
});
