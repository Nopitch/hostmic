//! Kernel Streaming data formats, as any process can send them to the capture pin
//! (`PROPOSEDATAFORMAT`, stream creation): never a panic, and every format the driver streams
//! is also one it accepts as a proposal.

#![no_main]

use hostmic_driver_core::format;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let supported = format::is_supported(data);
    let proposal = format::is_proposal_supported(data);
    assert!(!supported || proposal);
    if proposal {
        assert!(data.len() >= format::KSDATAFORMAT_BYTES);
    }
});
