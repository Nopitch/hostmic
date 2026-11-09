//! `hostmic-ctl`: inspects the hostmic driver through its host interface.
//!
//! ```text
//! hostmic-ctl info     every host interface: its path and what the driver offers
//! hostmic-ctl status   every host interface: the capture stream and the driver's counters
//! ```
//!
//! Plain text, one value per line with its unit, in the style of Virtual Audio Cable's control
//! panel (PLAN.md, "Host library"). Exit status: 0 on success, 1 when no interface answers, 2 on
//! a usage error.

use std::{env, process::ExitCode};

use hostmic::{
    Device, Error, Info, Reason, SampleType, State, Stats, StreamState, interfaces, request_name,
};

const USAGE: &str = "usage: hostmic-ctl info | status";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["info"] | [] => info(),
        ["status"] => status(),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// `hostmic-ctl info`.
fn info() -> ExitCode {
    let paths = match interfaces() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("cannot list host interfaces: {error}");
            return ExitCode::FAILURE;
        }
    };
    if paths.is_empty() {
        eprintln!("{}", Error::NotInstalled);
        return ExitCode::FAILURE;
    }
    let mut failed = false;
    for path in &paths {
        println!("hostmic host interface");
        println!("  path          {path}");
        match Device::open(path).and_then(|device| device.info()) {
            Ok(info) => print_info(&info),
            Err(error) => {
                println!("  error         {error}");
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// `hostmic-ctl status`.
fn status() -> ExitCode {
    let paths = match interfaces() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("cannot list host interfaces: {error}");
            return ExitCode::FAILURE;
        }
    };
    if paths.is_empty() {
        eprintln!("{}", Error::NotInstalled);
        return ExitCode::FAILURE;
    }
    let mut failed = false;
    for path in &paths {
        println!("hostmic host interface");
        println!("  path          {path}");
        let result = Device::open(path).and_then(|device| Ok((device.stream()?, device.stats()?)));
        match result {
            Ok((stream, stats)) => {
                print_stream(&stream);
                print_stats(&stats);
            }
            Err(error) => {
                println!("  error         {error}");
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn print_stream(stream: &StreamState) {
    let state = match State::from_code(stream.state) {
        Some(State::Stopped) => "stopped",
        Some(State::Paused) => "paused",
        Some(State::Running) => "running",
        None => "unknown",
    };
    println!("  stream        {state}, generation {}", stream.generation);
    let format = stream.format;
    if format.rate == 0 {
        println!("  format        none (no stream)");
        return;
    }
    println!(
        "  format        {} Hz, {} ch, {}",
        format.rate,
        format.channels,
        sample_types(SampleType::from_code(format.sample_type).map_or(0, SampleType::bit))
    );
    println!("  buffer        {} frames", stream.buffer_frames);
    if stream.frames_per_packet == 0 {
        println!("  packet        none (polling)");
    } else {
        println!("  packet        {} frames", stream.frames_per_packet);
    }
    println!("  position      {} frames", stream.position);
}

fn print_stats(stats: &Stats) {
    println!("  advanced      {} frames", stats.frames_advanced);
    println!("  completed     {} packets", stats.packets_completed);
    println!("  skipped       {} packets", stats.packets_skipped);
    println!("  refused       {} requests", stats.requests_refused);
    if stats.requests_refused != 0 {
        let reason = Reason::from_code(stats.last_reason).map_or_else(
            || format!("code {}", stats.last_reason),
            |r| format!("{r:?}"),
        );
        let request = request_name(stats.last_request).map_or_else(
            || format!("function {:#x}", stats.last_request),
            str::to_owned,
        );
        println!(
            "  last refusal  {request}: {reason}, status {:#010x}",
            stats.last_status
        );
    }
}

fn print_info(info: &Info) {
    let formats = info.formats;
    println!("  abi version   {}", info.abi_version);
    println!("  max buffer    {} bytes", info.max_buffer_bytes);
    println!(
        "  rates         {}",
        range(formats.min_rate, formats.max_rate, "Hz")
    );
    println!(
        "  channels      {}",
        range(
            u32::from(formats.min_channels),
            u32::from(formats.max_channels),
            "ch"
        )
    );
    println!("  sample types  {}", sample_types(formats.sample_types));
}

/// `48000 Hz`, or `44100..96000 Hz`.
fn range(min: u32, max: u32, unit: &str) -> String {
    if min == max {
        format!("{min} {unit}")
    } else {
        format!("{min}..{max} {unit}")
    }
}

/// The sample types of a [`hostmic_abi::FormatRange::sample_types`] mask, by name.
fn sample_types(mask: u32) -> String {
    let names: Vec<&str> = SampleType::ALL
        .into_iter()
        .filter(|sample_type| mask & sample_type.bit() != 0)
        .map(|sample_type| match sample_type {
            SampleType::Int16 => "int16",
            SampleType::Int24 => "int24",
            SampleType::Int32 => "int32",
            SampleType::Float32 => "float32",
        })
        .collect();
    if names.is_empty() {
        format!("none ({mask:#x})")
    } else {
        names.join(", ")
    }
}
