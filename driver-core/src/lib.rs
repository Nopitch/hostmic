//! # hostmic-driver-core
//!
//! The parts of the hostmic driver that are plain arithmetic on plain data: the stream format,
//! the test tone, the conversion from the performance counter to audio frames, the writes into
//! the capture ring buffer and the packet numbering of event-driven streams.
//!
//! None of it calls the kernel, so it builds and is unit-tested on the build machine
//! (`cargo test`), which the driver crate itself cannot be. Everything here is meant for the
//! driver's real-time path: no allocation, no lock, no I/O, no floating point at run time, no
//! panic for any input.

#![no_std]

pub mod clock;
pub mod format;
pub mod packet;
pub mod ring;
pub mod tone;
