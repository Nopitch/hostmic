//! # hostmic
//!
//! Feeds the hostmic virtual microphone from a user-mode application, the host: Windows
//! applications then record what the host writes, at the host's pace.
//!
//! This is the low layer: the host interface of `hostmic-abi` made safe.
//!
//! - [`interfaces`] lists the microphones' host interfaces; [`Device`] reads one (what the
//!   driver offers, the stream, the counters), from any process allowed to open it.
//! - [`Host::attach`] makes this process a microphone's host. The driver maps a ring buffer
//!   into it; [`Host::write`] copies frames into the ring and advances the stream position,
//!   [`Host::stream`] and [`Host::wait`] follow the stream's state. Dropping the [`Host`]
//!   detaches.
//!
//! A host writes in real time: call [`Host::write`] from a thread that does not allocate, lock
//! or perform I/O while the stream runs (PLAN.md, "Real-time rule"). `write` itself allocates
//! nothing.
//!
//! ```no_run
//! use hostmic::{Advance, FormatRange, Host, SampleType};
//!
//! # fn main() -> Result<(), hostmic::Error> {
//! let mut host = Host::attach(
//!     &hostmic::first_interface()?,
//!     FormatRange {
//!         min_rate: 48_000,
//!         max_rate: 48_000,
//!         min_channels: 2,
//!         max_channels: 2,
//!         sample_types: SampleType::Int16.bit(),
//!     },
//! )?;
//! let silence = [0u8; 48 * 4]; // 1 ms of 16-bit stereo at 48 kHz
//! let mut state = host.stream()?;
//! let mut position = state.position;
//! loop {
//!     // A real host writes the frames due by its clock, about every millisecond.
//!     match host.write(&state, position, &silence)? {
//!         Advance::Advanced(next) => position = next,
//!         Advance::Stale => {
//!             state = host.stream()?;
//!             position = state.position;
//!         }
//!         Advance::NotRunning => {
//!             host.wait(std::time::Duration::from_millis(100))?;
//!             state = host.stream()?;
//!             position = state.position;
//!         }
//!     }
//! #   break;
//! }
//! # Ok(())
//! # }
//! ```

// The driver maps the buffer only into 64-bit processes and refuses to attach a 32-bit one.
#[cfg(not(target_pointer_width = "64"))]
compile_error!("hostmic hosts are 64-bit processes: the driver refuses to attach 32-bit ones");

mod device;
mod error;
mod ring;
pub mod stream;

pub use device::{Advance, Device, Host, first_interface, interfaces};
pub use error::Error;
pub use hostmic_abi::{
    ABI_VERSION, Format, FormatRange, Info, Reason, SampleType, State, Stats, StreamState,
};

/// The name of a request by its control code's function number (as
/// [`Stats::last_request`] reports it), or `None` for an unknown one.
#[must_use]
pub fn request_name(function: u16) -> Option<&'static str> {
    use hostmic_abi::{
        IOCTL_ADVANCE, IOCTL_ATTACH, IOCTL_DETACH, IOCTL_GET_INFO, IOCTL_GET_STATS,
        IOCTL_GET_STREAM,
    };
    [
        (IOCTL_GET_INFO, "GET_INFO"),
        (IOCTL_ATTACH, "ATTACH"),
        (IOCTL_GET_STREAM, "GET_STREAM"),
        (IOCTL_ADVANCE, "ADVANCE"),
        (IOCTL_DETACH, "DETACH"),
        (IOCTL_GET_STATS, "GET_STATS"),
    ]
    .into_iter()
    .find(|(code, _)| hostmic_abi::function(*code) == function)
    .map(|(_, name)| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_names() {
        assert_eq!(
            request_name(hostmic_abi::function(hostmic_abi::IOCTL_ADVANCE)),
            Some("ADVANCE")
        );
        assert_eq!(request_name(0x123), None);
    }
}
