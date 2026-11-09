//! # hostmic-driver-core
//!
//! The parts of the hostmic driver that are plain arithmetic on plain data: the stream format,
//! the conversion between the performance counter and audio frames, the packet numbering of
//! event-driven streams, and, for the host interface, the validation of every request and the
//! geometry of the shared buffer.
//!
//! None of it calls the kernel, so it builds and is unit-tested on the build machine
//! (`cargo test`), which the driver crate itself cannot be. Everything here is meant for the
//! driver's real-time path: no allocation, no lock, no I/O, no floating point at run time, no
//! panic for any input.

#![no_std]

pub mod buffer;
pub mod clock;
pub mod format;
pub mod host;
pub mod packet;
