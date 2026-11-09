//! `ATTACH` inputs: never a panic, and whatever is accepted is a well-formed request whose
//! event handle value is in range and whose formats are ones the driver supports.

#![no_main]

use hostmic_abi::{ABI_VERSION, AttachRequest};
use hostmic_driver_core::host::{self, EVENT_MAX, Rejection, SUPPORTED};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    assert_eq!(host::attach(data, true), Err(Rejection::Wow64));
    host::no_input(data).ok();

    let Ok(attach) = host::attach(data, false) else {
        return;
    };
    let bytes: &[u8; AttachRequest::SIZE] = data.try_into().expect("accepted input size");
    let request = AttachRequest::from_bytes(bytes);
    assert_eq!(request.abi_version, ABI_VERSION);
    assert_eq!(request.reserved, 0);
    assert_eq!(u64::from(attach.event), request.event);
    assert!(attach.event != 0 && u64::from(attach.event) <= EVENT_MAX);

    let formats = attach.formats;
    assert!(formats.is_valid());
    assert!(formats.min_rate >= SUPPORTED.min_rate && formats.max_rate <= SUPPORTED.max_rate);
    assert!(
        formats.min_channels >= SUPPORTED.min_channels
            && formats.max_channels <= SUPPORTED.max_channels
    );
    assert_eq!(formats.sample_types & !SUPPORTED.sample_types, 0);
    // The offered formats are also ones the host asked for.
    assert_eq!(formats.intersect(&request.formats), Some(formats));
});
