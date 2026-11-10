//! The errors of the host crate.

use core::fmt;

use hostmic_abi::FormatRange;

/// Why a host operation failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// No host interface is present: the driver is not installed, or its device not started.
    NotInstalled,
    /// The driver speaks another version of the host interface.
    Version {
        /// The driver's [`ABI_VERSION`](hostmic_abi::ABI_VERSION).
        driver: u32,
        /// This crate's.
        host: u32,
    },
    /// The host's format range is empty, inverted or names an unknown sample type.
    InvalidRange(FormatRange),
    /// None of the host's formats is one the driver streams.
    NoCommonFormat {
        /// What the host asked for.
        host: FormatRange,
        /// What the driver streams.
        driver: FormatRange,
    },
    /// Another host is attached to this microphone.
    Busy,
    /// This handle is no longer the attached host: it detached, the driver detached it, or the
    /// host interface was removed (the host should let go: `Player::follow` does).
    NotHost,
    /// A write the stream cannot take.
    Write(&'static str),
    /// The driver's reply is not the size the host interface specifies.
    Reply {
        /// The request.
        request: &'static str,
        /// Bytes returned.
        returned: u32,
        /// Bytes expected.
        expected: u32,
    },
    /// The driver mapped the buffer at an unusable address.
    Mapping {
        /// The address `ATTACH` returned.
        address: u64,
        /// The size it returned, in bytes.
        bytes: u32,
    },
    /// A system call failed.
    Os {
        /// The call, or the request for `DeviceIoControl`.
        call: &'static str,
        /// What it returned.
        error: windows::core::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled => {
                f.write_str("no hostmic host interface: is the driver installed and started?")
            }
            Self::Version { driver, host } => write!(
                f,
                "the driver speaks host interface version {driver}, this host version {host}"
            ),
            Self::InvalidRange(range) => write!(f, "invalid format range: {range:?}"),
            Self::NoCommonFormat { host, driver } => write!(
                f,
                "no common format: the host writes {host:?}, the driver streams {driver:?}"
            ),
            Self::Busy => f.write_str("another host is attached to this microphone"),
            Self::NotHost => f.write_str("this handle is no longer the attached host"),
            Self::Write(reason) => write!(f, "write refused: {reason}"),
            Self::Reply {
                request,
                returned,
                expected,
            } => write!(
                f,
                "{request} returned {returned} bytes, {expected} expected"
            ),
            Self::Mapping { address, bytes } => {
                write!(f, "unusable buffer mapping: {bytes} bytes at {address:#x}")
            }
            Self::Os { call, error } => write!(f, "{call}: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Os { error, .. } => Some(error),
            _ => None,
        }
    }
}
