//! `test-host`: a test host for the hostmic driver, on the `hostmic` crate's reference loop.
//!
//! ```text
//! test-host [--seconds N]
//! ```
//!
//! Attaches to the first host interface and plays a 1 kHz tone at -6.02 dBFS for `N` seconds
//! (default 30) with a [`hostmic::Player`]: a thread registered with MMCSS writes the frames
//! due by the performance counter, about a millisecond per request, while the stream runs.
//! The driver produces no audio and keeps no clock of its own: what `tools/capture-check`
//! records is what this host wrote, at its pace. Once a second it prints the player's
//! counters, and the stream (read through a second, read-only handle) whenever it changed; at
//! the end, the driver's counters, then it detaches. The driver's handle rules (closing
//! without `DETACH`, inherited and duplicated handles) are checked by `tools/handle-check`.
//!
//! Exit status: 0 on success, 1 on failure, 2 on a usage error.

use std::{
    env,
    process::ExitCode,
    thread,
    time::{Duration, Instant},
};

use hostmic::{
    Block, Device, FormatRange, Host, Player, PlayerStats, SAMPLE_TYPES_ALL, SampleType, State,
    StreamState,
};

const USAGE: &str = "usage: test-host [--seconds N]";

/// Command-line options.
struct Options {
    seconds: u64,
}

fn parse() -> Result<Options, String> {
    let mut options = Options { seconds: 30 };
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seconds" => {
                let value = args.next().ok_or("--seconds needs a value")?;
                options.seconds = value
                    .parse()
                    .map_err(|_| format!("--seconds: not a number: {value}"))?;
            }
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }
    Ok(options)
}

fn main() -> ExitCode {
    let options = match parse() {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(options: &Options) -> Result<(), hostmic::Error> {
    let path = hostmic::first_interface()?;
    println!("interface  {path}");
    // Any format: the microphone offers what the driver streams.
    let host = Host::attach(
        &path,
        FormatRange {
            min_rate: 8_000,
            max_rate: 384_000,
            min_channels: 1,
            max_channels: 32,
            sample_types: SAMPLE_TYPES_ALL,
        },
    )?;
    let formats = host.formats();
    println!(
        "attached   {} bytes mapped; offered {}..{} Hz, {}..{} ch, sample types {:#x}",
        host.buffer_bytes(),
        formats.min_rate,
        formats.max_rate,
        formats.min_channels,
        formats.max_channels,
        formats.sample_types
    );

    // The player owns the host: the stream is followed through a read-only handle.
    let device = Device::open(&path)?;
    let player = Player::start(host, tone)?;
    let end = Instant::now() + Duration::from_secs(options.seconds);
    let mut stream = None;
    while Instant::now() < end {
        thread::sleep(Duration::from_secs(1));
        let now = device.stream()?;
        if stream != Some(now) {
            print_stream(&now);
            stream = Some(now);
        }
        let counters = player.stats();
        print_stats(&counters);
        if counters.finished {
            break;
        }
    }
    let host = player.stop()?;

    let driver = host.stats()?;
    println!(
        "driver     {} frames advanced, {} packets completed, {} skipped, {} requests refused",
        driver.frames_advanced,
        driver.packets_completed,
        driver.packets_skipped,
        driver.requests_refused
    );
    host.detach()?;
    println!("detached");
    Ok(())
}

/// One line per stream state, with units. Sampled once a second: a quick stop and start shows
/// in the player's `stale` counter instead.
fn print_stream(stream: &StreamState) {
    let state = match State::from_code(stream.state) {
        Some(State::Stopped) => "stopped",
        Some(State::Paused) => "paused",
        Some(State::Running) => "running",
        None => "unknown",
    };
    let format = stream.format;
    if format.rate == 0 {
        println!(
            "stream     gen {:>5}  {state:<8} no stream",
            stream.generation
        );
        return;
    }
    let sample = match SampleType::from_code(format.sample_type) {
        Some(SampleType::Int16) => "int16",
        Some(SampleType::Int24) => "int24",
        Some(SampleType::Int32) => "int32",
        Some(SampleType::Float32) => "float32",
        None => "?",
    };
    println!(
        "stream     gen {:>5}  {state:<8} {} Hz {} ch {sample}  buffer {} frames  packet {} frames  position {}",
        stream.generation,
        format.rate,
        format.channels,
        stream.buffer_frames,
        stream.frames_per_packet,
        stream.position
    );
}

/// The player's callback: a 1 kHz sine at half of full scale (-6.02 dBFS) on every channel,
/// the signal `tools/capture-check` expects, with its phase taken from the stream position so
/// that it stays continuous across blocks.
fn tone(block: &mut Block<'_>) {
    let frames_per_second = block.format().rate.max(1);
    let rate = f64::from(frames_per_second);
    let channels = block.channels().max(1);
    let start = block.position();
    for (index, frame) in block.samples_mut().chunks_mut(channels).enumerate() {
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            reason = "the frame number modulo the rate is far below 2^52; |0.5 sin| fits an f32"
        )]
        let value = {
            let n = (start + index as u64) % u64::from(frames_per_second);
            (0.5 * (core::f64::consts::TAU * 1_000.0 * n as f64 / rate).sin()) as f32
        };
        frame.fill(value);
    }
}

fn print_stats(stats: &PlayerStats) {
    println!(
        "playing    {} frames  {} advances  {} stale  {} late  longest gap {:.1} ms{}",
        stats.frames_written,
        stats.advances,
        stats.stale,
        stats.late,
        stats.longest_gap.as_secs_f64() * 1_000.0,
        if stats.mmcss { "" } else { "  (no MMCSS)" }
    );
}
