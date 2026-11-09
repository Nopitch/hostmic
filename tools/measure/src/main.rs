//! # measure
//!
//! M5's measurements, after Résonnance's `tag_micro`: one process plays into the hostmic
//! microphone with the `hostmic` crate's reference loop (`Player::follow`) and records it with
//! a reader that does what communication applications do (shared-mode, event-driven WASAPI on
//! an MMCSS thread). Both share the performance counter.
//!
//! ```text
//! measure latency [--seconds N] [--threads N] [--name <substring>]
//! measure load [--seconds N] [--threads N] [--name <substring>]
//! ```
//!
//! - `latency`: a 1 ms click every half second, after a second of silence. The callback notes
//!   the performance counter when it writes a click's first frame, the reader when it gets the
//!   packet that holds the click's rising edge; the difference is the write → read latency an
//!   application sees, packetization included (the click sits anywhere in a packet of the
//!   engine's period). Reports the median, 95th percentile and maximum. Passes when at least
//!   80 % of the clicks due were read.
//! - `load`: a 440 Hz sine at -6 dBFS while `--threads` threads (default 16, the target's
//!   load) spin; the reader counts crackles (a sample the sine does not predict, or 2 ms of
//!   silence). Passes with no crackle and at least 90 % of the recording audible. A long
//!   `--seconds` makes it an endurance run.
//!
//! Both also fail on an engine discontinuity, a wait for a packet that timed out, or less than
//! 90 % of the frames due read. Both print the player's counters, the distribution of the gaps
//! between its writing wake-ups, and the driver's counters. Exit status: 0 on a pass, 1 on a
//! fail, 2 on an error.

mod detect;

use std::{
    fmt::Write as _,
    process::ExitCode,
    ptr, slice,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use hostmic::{Block, FormatRange, GAP_EDGES_US, Player, PlayerStats, SAMPLE_TYPES_ALL};
use windows::{
    Win32::{
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
        Media::Audio::{
            AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
            AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, DEVICE_STATE_ACTIVE,
            IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
            WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eCapture,
        },
        System::{
            Com::{
                CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
                STGM_READ, StructuredStorage::PropVariantToBSTR,
            },
            Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
            Threading::{AvSetMmThreadCharacteristicsW, CreateEventW, WaitForSingleObject},
        },
    },
    core::{GUID, PCWSTR, w},
};

use crate::detect::{Crackles, Edges, summary};

/// `WAVE_FORMAT_IEEE_FLOAT` (`mmreg.h`).
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
/// `WAVE_FORMAT_EXTENSIBLE` (`mmreg.h`).
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT` (`ksmedia.h`).
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x0000_0003_0000_0010_8000_00aa_0038_9b71);

/// The load mode's sine.
const SINE_HZ: f64 = 440.0;

const USAGE: &str =
    "usage: measure (latency | load) [--seconds N] [--threads N] [--name <substring>]";

/// What to measure.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Latency,
    Load,
}

/// Command-line options.
struct Options {
    mode: Mode,
    seconds: u64,
    threads: usize,
    name: String,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mode = match args.next().as_deref() {
        Some("latency") => Mode::Latency,
        Some("load") => Mode::Load,
        _ => return Err("latency or load first".into()),
    };
    let mut options = Options {
        mode,
        seconds: 30,
        threads: if mode == Mode::Load { 16 } else { 0 },
        name: "hostmic".into(),
    };
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--seconds" => {
                options.seconds = value()?.parse().map_err(|e| format!("--seconds: {e}"))?;
            }
            "--threads" => {
                options.threads = value()?.parse().map_err(|e| format!("--threads: {e}"))?;
            }
            "--name" => options.name = value()?,
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    if !(3..=86_400).contains(&options.seconds) {
        return Err("--seconds must be between 3 and 86400".into());
    }
    if options.threads > 256 {
        return Err("--threads must be at most 256".into());
    }
    Ok(options)
}

fn main() -> ExitCode {
    let options = match parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("measure: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&options) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(message) => {
            eprintln!("measure: {message}");
            ExitCode::from(2)
        }
    }
}

/// The performance counter.
fn qpc() -> i64 {
    let mut now = 0;
    // SAFETY: a writable counter; cannot fail on Windows XP and later.
    let _ = unsafe { QueryPerformanceCounter(&raw mut now) };
    now
}

/// What the reader saw.
#[derive(Default)]
struct Reading {
    rate: u32,
    channels: usize,
    endpoint: String,
    packets: u64,
    /// Packets flagged `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`, the first one excepted.
    discontinuities: u64,
    silent_packets: u64,
    timeouts: u64,
    /// Frames read, and how long the reading lasted, in seconds.
    frames: u64,
    seconds: f64,
    /// Frames read after the first two seconds, and those of them audible.
    later_frames: u64,
    audible: u64,
    /// Write → read latencies, in milliseconds.
    latencies: Vec<f64>,
    crackles: u64,
    crackle_silences: u64,
}

fn run(options: &Options) -> Result<bool, String> {
    let mut frequency = 0;
    // SAFETY: a writable frequency.
    unsafe { QueryPerformanceFrequency(&raw mut frequency) }
        .map_err(|e| format!("QueryPerformanceFrequency: {e}"))?;
    // The performance counter at the last click's first frame, written by the callback.
    let click = Arc::new(AtomicI64::new(0));
    let player = Player::follow(
        FormatRange {
            min_rate: 8_000,
            max_rate: 384_000,
            min_channels: 1,
            max_channels: 32,
            sample_types: SAMPLE_TYPES_ALL,
        },
        source(options.mode, Arc::clone(&click)),
    )
    .map_err(|e| format!("Player::follow: {e}"))?;

    let stop = Arc::new(AtomicBool::new(false));
    let load: Vec<_> = (0..options.threads)
        .map(|_| {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut x = 1.0f64;
                while !stop.load(Ordering::Relaxed) {
                    for _ in 0..10_000 {
                        x = std::hint::black_box(x * 1.000_000_1 + 1e-9);
                    }
                }
            })
        })
        .collect();
    let reading = read(options, &click, frequency);
    stop.store(true, Ordering::Relaxed);
    for thread in load {
        let _ = thread.join();
    }
    let stats = player.stats();
    // The reading is reported even when the player ended on an error, which fails the run.
    let (driver, player_error) = match player.stop() {
        Ok(Some(host)) => {
            let stats = host.stats().ok();
            let _ = host.detach();
            (stats, None)
        }
        Ok(None) => (None, None),
        Err(error) => (None, Some(error)),
    };
    let mut reading = reading?;
    Ok(report(
        options,
        &mut reading,
        &stats,
        driver.as_ref(),
        player_error.as_ref(),
    ))
}

/// The player's callback for `mode`: clicks, or the sine. Allocation-free.
fn source(mode: Mode, click: Arc<AtomicI64>) -> impl FnMut(&mut Block<'_>) + Send + 'static {
    move |block: &mut Block<'_>| {
        let rate = u64::from(block.format().rate.max(1));
        let channels = block.channels().max(1);
        let start = block.position();
        // A 1 ms click every half second, after a second of silence.
        let (first, every, length) = (rate, rate / 2, rate.div_ceil(1_000));
        for (index, frame) in block.samples_mut().chunks_mut(channels).enumerate() {
            let n = start + index as u64;
            let value = match mode {
                Mode::Latency => {
                    let into = n.checked_sub(first).map(|n| n % every.max(1));
                    if into == Some(0) {
                        click.store(qpc(), Ordering::Relaxed);
                    }
                    if into.is_some_and(|into| into < length) {
                        0.8
                    } else {
                        0.0
                    }
                }
                Mode::Load => {
                    #[allow(
                        clippy::cast_precision_loss,
                        clippy::cast_possible_truncation,
                        reason = "n modulo the rate is far below 2^52; |0.5 sin| fits an f32"
                    )]
                    let y = (0.5
                        * (core::f64::consts::TAU * SINE_HZ * (n % rate) as f64 / rate as f64)
                            .sin()) as f32;
                    y
                }
            };
            frame.fill(value);
        }
    }
}

/// Records the microphone for `options.seconds` seconds, as a communication application does,
/// and analyzes what it reads.
fn read(options: &Options, click: &AtomicI64, frequency: i64) -> Result<Reading, String> {
    // SAFETY: the first COM call of this thread; it stays in the MTA until it exits.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .map_err(|e| format!("CoInitializeEx: {e}"))?;
    let mut task = 0u32;
    // SAFETY: a static task name and a writable index. Failure only loses priority.
    let _ = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &raw mut task) };
    // The endpoint is active once the player has attached (its jack is plugged in then).
    let waiting = Instant::now();
    let (device, endpoint) = loop {
        match find_endpoint(&options.name) {
            Ok(found) => break found,
            Err(error) if waiting.elapsed() > Duration::from_secs(20) => return Err(error),
            Err(_) => thread::sleep(Duration::from_millis(250)),
        }
    };
    // SAFETY: a live endpoint; no activation parameters.
    let client: IAudioClient =
        unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|e| format!("Activate: {e}"))?;
    // SAFETY: a live, uninitialized client.
    let format = unsafe { client.GetMixFormat() }.map_err(|e| format!("GetMixFormat: {e}"))?;
    let result = record(&client, format, options, click, frequency);
    // SAFETY: allocated by `GetMixFormat` with `CoTaskMemAlloc`; not used afterwards.
    unsafe { CoTaskMemFree(Some(format.cast_const().cast())) };
    result.map(|mut reading| {
        reading.endpoint = endpoint;
        reading
    })
}

/// The first active capture endpoint whose friendly name contains `needle` (any case).
fn find_endpoint(needle: &str) -> Result<(IMMDevice, String), String> {
    // SAFETY: COM is initialized on this thread.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| format!("MMDeviceEnumerator: {e}"))?;
    // SAFETY: a live enumerator.
    let devices = unsafe { enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) }
        .map_err(|e| format!("EnumAudioEndpoints: {e}"))?;
    // SAFETY: a live collection.
    let count = unsafe { devices.GetCount() }.map_err(|e| format!("GetCount: {e}"))?;
    let needle = needle.to_lowercase();
    let mut seen = String::new();
    for i in 0..count {
        // SAFETY: `i` is below the collection's count.
        let device = unsafe { devices.Item(i) }.map_err(|e| format!("Item: {e}"))?;
        // SAFETY: a live endpoint.
        let store = unsafe { device.OpenPropertyStore(STGM_READ) }
            .map_err(|e| format!("properties: {e}"))?;
        // SAFETY: a live store; a valid key.
        let value = unsafe { store.GetValue(&PKEY_Device_FriendlyName) }
            .map_err(|e| format!("friendly name: {e}"))?;
        // SAFETY: a valid PROPVARIANT owned here.
        let name = unsafe { PropVariantToBSTR(&raw const value) }
            .map_err(|e| format!("friendly name: {e}"))?
            .to_string();
        if name.to_lowercase().contains(&needle) {
            return Ok((device, name));
        }
        let _ = write!(seen, "\n  {name}");
    }
    Err(format!(
        "no active capture endpoint matches \"{needle}\"; active ones:{}",
        if seen.is_empty() { " none" } else { &seen }
    ))
}

/// Initializes `client` on its shared-mode float mix format and records.
fn record(
    client: &IAudioClient,
    format: *mut WAVEFORMATEX,
    options: &Options,
    click: &AtomicI64,
    frequency: i64,
) -> Result<Reading, String> {
    // SAFETY: `GetMixFormat` returned a valid, possibly unaligned (packed) WAVEFORMATEX.
    let wave = unsafe { format.read_unaligned() };
    let tag = wave.wFormatTag;
    let float = match tag {
        WAVE_FORMAT_IEEE_FLOAT => true,
        WAVE_FORMAT_EXTENSIBLE => {
            // SAFETY: a WAVE_FORMAT_EXTENSIBLE mix format is a full WAVEFORMATEXTENSIBLE.
            let extensible = unsafe { format.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned() };
            let sub_format = extensible.SubFormat;
            sub_format == SUBTYPE_IEEE_FLOAT
        }
        _ => false,
    };
    let (rate, channels, bits) = (wave.nSamplesPerSec, wave.nChannels, wave.wBitsPerSample);
    if !float || bits != 32 || channels == 0 {
        return Err("expected a 32-bit float mix format (shared mode)".into());
    }
    // SAFETY: a live client, not initialized yet; `format` is its own mix format.
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            0,
            0,
            format,
            None,
        )
    }
    .map_err(|e| format!("Initialize: {e}"))?;
    // SAFETY: an unnamed auto-reset event.
    let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
        .map_err(|e| format!("CreateEventW: {e}"))?;
    let mut reading = Reading {
        rate,
        channels: usize::from(channels),
        ..Reading::default()
    };
    let result = capture(client, event, options, click, frequency, &mut reading);
    // SAFETY: a handle created above; the client no longer signals it.
    let _ = unsafe { CloseHandle(event) };
    result.map(|()| reading)
}

/// The capture loop, until `options.seconds` have passed.
fn capture(
    client: &IAudioClient,
    event: HANDLE,
    options: &Options,
    click: &AtomicI64,
    frequency: i64,
    reading: &mut Reading,
) -> Result<(), String> {
    // SAFETY: a client initialized for event callbacks; a valid event.
    unsafe { client.SetEventHandle(event) }.map_err(|e| format!("SetEventHandle: {e}"))?;
    // SAFETY: an initialized client.
    let capture: IAudioCaptureClient =
        unsafe { client.GetService() }.map_err(|e| format!("GetService: {e}"))?;
    let frame_bytes = reading.channels * 4;
    let mut crackles = Crackles::new(SINE_HZ, reading.rate);
    let mut edges = Edges::default();
    #[allow(clippy::cast_precision_loss, reason = "a frequency far below 2^52")]
    let ticks_per_ms = frequency as f64 / 1_000.0;
    // The click last paired with an edge: a second edge in the same click (a dropout inside
    // it) is not paired again.
    let mut paired = 0i64;
    let warm_up = 2 * u64::from(reading.rate);
    let started = Instant::now();
    let end = started + Duration::from_secs(options.seconds);
    // SAFETY: an initialized client with its event.
    unsafe { client.Start() }.map_err(|e| format!("Start: {e}"))?;
    while Instant::now() < end {
        // SAFETY: a valid event handle.
        if unsafe { WaitForSingleObject(event, 2_000) } != WAIT_OBJECT_0 {
            reading.timeouts += 1;
            continue;
        }
        loop {
            // SAFETY: a live capture client.
            let size = unsafe { capture.GetNextPacketSize() }
                .map_err(|e| format!("GetNextPacketSize: {e}"))?;
            if size == 0 {
                break;
            }
            let (mut data, mut frames, mut flags) = (ptr::null_mut(), 0, 0);
            // SAFETY: valid out-parameters; positions not requested.
            unsafe {
                capture.GetBuffer(&raw mut data, &raw mut frames, &raw mut flags, None, None)
            }
            .map_err(|e| format!("GetBuffer: {e}"))?;
            let now = qpc();
            reading.packets += 1;
            if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0.cast_unsigned() != 0
                && reading.packets > 1
            {
                reading.discontinuities += 1;
            }
            let count = frames as usize;
            let silent =
                flags & AUDCLNT_BUFFERFLAGS_SILENT.0.cast_unsigned() != 0 || data.is_null();
            if silent {
                reading.silent_packets += 1;
            }
            let packet: &[u8] = if silent {
                &[]
            } else {
                // SAFETY: `GetBuffer` returned `frames` frames of `frame_bytes` bytes at `data`,
                // valid until `ReleaseBuffer`, below.
                unsafe { slice::from_raw_parts(data, count * frame_bytes) }
            };
            for index in 0..count {
                // The first channel's float; 0 in a silent packet.
                let y = packet
                    .get(index * frame_bytes..index * frame_bytes + 4)
                    .map_or(0.0, |b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                reading.frames += 1;
                if reading.frames > warm_up {
                    reading.later_frames += 1;
                    if y.abs() >= 0.01 {
                        reading.audible += 1;
                    }
                }
                match options.mode {
                    Mode::Load => crackles.push(y),
                    Mode::Latency => {
                        let written = click.load(Ordering::Relaxed);
                        if edges.push(y) && written != 0 && written != paired && now > written {
                            paired = written;
                            #[allow(
                                clippy::cast_precision_loss,
                                reason = "a few seconds of ticks, far below 2^52"
                            )]
                            reading
                                .latencies
                                .push((now - written) as f64 / ticks_per_ms);
                        }
                    }
                }
            }
            // SAFETY: releases exactly the packet obtained above.
            unsafe { capture.ReleaseBuffer(frames) }.map_err(|e| format!("ReleaseBuffer: {e}"))?;
        }
    }
    // SAFETY: started above.
    unsafe { client.Stop() }.map_err(|e| format!("Stop: {e}"))?;
    reading.seconds = started.elapsed().as_secs_f64();
    reading.crackles = crackles.count;
    reading.crackle_silences = crackles.silences;
    Ok(())
}

/// Prints everything measured; returns whether it passes.
fn report(
    options: &Options,
    reading: &mut Reading,
    stats: &PlayerStats,
    driver: Option<&hostmic::Stats>,
    player_error: Option<&hostmic::Error>,
) -> bool {
    println!(
        "measure    {}, {} s, {} load thread(s)",
        if options.mode == Mode::Latency {
            "latency"
        } else {
            "load"
        },
        options.seconds,
        options.threads
    );
    println!(
        "endpoint   {} ({} Hz, {} ch)",
        reading.endpoint, reading.rate, reading.channels
    );
    #[allow(
        clippy::cast_precision_loss,
        reason = "frame counts and rates far below 2^52"
    )]
    let (due, frames, audible) = (
        reading.seconds * f64::from(reading.rate),
        reading.frames as f64,
        reading.audible as f64 / reading.later_frames.max(1) as f64,
    );
    println!(
        "engine     {} packets, {} discontinuities (after the first), {} silent, {} timeouts; {} frames read in {:.1} s ({:.1} % of those due)",
        reading.packets,
        reading.discontinuities,
        reading.silent_packets,
        reading.timeouts,
        reading.frames,
        reading.seconds,
        100.0 * frames / due.max(1.0)
    );
    let healthy = reading.discontinuities == 0 && reading.timeouts == 0 && frames >= 0.9 * due;
    let passed = healthy
        && match options.mode {
            Mode::Latency => {
                let count = reading.latencies.len();
                // A click every half second, from one second in.
                let expected = (2.0 * (reading.seconds - 1.0)).max(1.0);
                if let Some((median, p95, max)) = summary(&mut reading.latencies) {
                    println!(
                        "latency    write -> read, {count} clicks ({expected:.0} due): median {median:.1} ms, 95th percentile {p95:.1} ms, max {max:.1} ms"
                    );
                    #[allow(clippy::cast_precision_loss, reason = "a small count")]
                    let enough = count as f64 >= 0.8 * expected;
                    enough
                } else {
                    println!("latency    no click read");
                    false
                }
            }
            Mode::Load => {
                println!(
                    "load       {} crackle(s), of which {} silence(s); {:.1} % audible after the warm-up",
                    reading.crackles,
                    reading.crackle_silences,
                    100.0 * audible
                );
                reading.crackles == 0 && audible >= 0.9
            }
        };
    println!(
        "player     {} frames, {} advances, {} stale, {} late, longest gap {:.1} ms, {} attachment(s), {} release(s){}",
        stats.frames_written,
        stats.advances,
        stats.stale,
        stats.late,
        stats.longest_gap.as_secs_f64() * 1_000.0,
        stats.attachments,
        stats.releases,
        if stats.mmcss { "" } else { ", no MMCSS" }
    );
    let mut gaps = String::new();
    for (bucket, count) in stats.gaps.iter().enumerate() {
        let ms = |edge: u32| f64::from(edge) / 1_000.0;
        let label = match bucket {
            0 => format!("<{} ms", ms(GAP_EDGES_US[0])),
            b if b == GAP_EDGES_US.len() => format!(">={} ms", ms(GAP_EDGES_US[b - 1])),
            b => format!("{}-{} ms", ms(GAP_EDGES_US[b - 1]), ms(GAP_EDGES_US[b])),
        };
        let _ = write!(gaps, "  {label}: {count}");
    }
    println!("gaps       {}", gaps.trim_start());
    match driver {
        Some(driver) => println!(
            "driver     {} frames advanced, {} packets completed, {} skipped, {} requests refused",
            driver.frames_advanced,
            driver.packets_completed,
            driver.packets_skipped,
            driver.requests_refused
        ),
        None => println!("driver     not attached at the end"),
    }
    if let Some(error) = player_error {
        println!("player     ended on an error: {error}");
    }
    let passed = passed && player_error.is_none();
    println!("{}", if passed { "PASS" } else { "FAIL" });
    passed
}
