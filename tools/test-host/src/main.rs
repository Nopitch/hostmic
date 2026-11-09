//! `test-host`: a test host for the hostmic driver, on the `hostmic` crate's reference loop.
//!
//! ```text
//! test-host [--seconds N]
//! ```
//!
//! Plays a 1 kHz tone at -6.02 dBFS for `N` seconds (default 30) with a
//! [`hostmic::Player`] following the first hostmic microphone: it attaches as soon as the
//! microphone is there, lets it go when Windows asks to remove it (so that `devcon remove` or
//! a driver update goes through), and attaches again when it comes back. While attached and
//! the stream runs, a thread registered with MMCSS writes the frames due by the performance
//! counter, about a millisecond per request. The driver produces no audio and keeps no clock
//! of its own: what `tools/capture-check` records is what this host wrote, at its pace.
//!
//! Once a second it prints the player's counters, and a line whenever the attachment or the
//! stream's state changed; at the end, the driver's counters if still attached, then it
//! detaches. It keeps no other handle to the microphone, which would hold its removal back.
//! The driver's handle rules are checked by `tools/handle-check`.
//!
//! Exit status: 0 on success, 1 on failure, 2 on a usage error.

use std::{
    env,
    process::ExitCode,
    thread,
    time::{Duration, Instant},
};

use hostmic::{Block, FormatRange, Player, PlayerStats, SAMPLE_TYPES_ALL, State};

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
    // Any format: the microphone offers what the driver streams.
    let formats = FormatRange {
        min_rate: 8_000,
        max_rate: 384_000,
        min_channels: 1,
        max_channels: 32,
        sample_types: SAMPLE_TYPES_ALL,
    };
    let player = Player::follow(formats, tone)?;
    let end = Instant::now() + Duration::from_secs(options.seconds);
    let mut seen = (false, None, 0);
    while Instant::now() < end {
        thread::sleep(Duration::from_secs(1));
        let counters = player.stats();
        let now = (counters.attached, counters.state, counters.releases);
        if now != seen {
            println!(
                "host       {}, stream {}  ({} attachments, {} releases)",
                if counters.attached {
                    "attached"
                } else {
                    "waiting for the microphone"
                },
                state_name(counters.state),
                counters.attachments,
                counters.releases
            );
            seen = now;
        }
        print_stats(&counters);
        if counters.finished {
            break;
        }
    }
    match player.stop()? {
        Some(host) => {
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
        }
        None => println!("not attached at the end"),
    }
    Ok(())
}

fn state_name(state: Option<State>) -> &'static str {
    match state {
        Some(State::Stopped) => "stopped",
        Some(State::Paused) => "paused",
        Some(State::Running) => "running",
        None => "none",
    }
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
