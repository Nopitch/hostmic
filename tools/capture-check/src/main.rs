//! # capture-check
//!
//! Records a capture endpoint (by default the first whose name contains "hostmic") through
//! shared-mode, event-driven WASAPI, as most recording applications do, and checks that it
//! carries a clean sine: frequency, level, discontinuities reported by the audio engine, and
//! glitches in the signal itself. Used in the test VM for milestone M3 (the driver plays a
//! 1 kHz tone); later the basis of the host's measurements.
//!
//! ```text
//! capture-check [--name <substring>] [--seconds <n>] [--expect <hz>]
//! ```
//!
//! Exit code: 0 when the recording passes, 1 when it does not, 2 on error.

mod analysis;

use std::{fmt::Write as _, process::ExitCode, ptr, slice};

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
            Threading::{CreateEventW, WaitForSingleObject},
        },
    },
    core::{GUID, PCWSTR},
};

/// `WAVE_FORMAT_IEEE_FLOAT` (`mmreg.h`).
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
/// `WAVE_FORMAT_EXTENSIBLE` (`mmreg.h`).
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT` (`ksmedia.h`).
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x0000_0003_0000_0010_8000_00aa_0038_9b71);

/// Start of the recording left out of the analysis: the audio engine's first packets.
const SETTLE_SECONDS: f64 = 0.1;

/// Command-line options.
struct Options {
    name: String,
    seconds: u32,
    expect: f64,
}

impl Options {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut options = Self {
            name: "hostmic".into(),
            seconds: 10,
            expect: 1_000.0,
        };
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or(format!("{arg} needs a value"));
            match arg.as_str() {
                "--name" => options.name = value()?,
                "--seconds" => {
                    options.seconds = value()?.parse().map_err(|e| format!("--seconds: {e}"))?;
                }
                "--expect" => {
                    options.expect = value()?.parse().map_err(|e| format!("--expect: {e}"))?;
                }
                _ => {
                    return Err(format!(
                        "unknown argument {arg}\nusage: capture-check [--name <substring>] \
                         [--seconds <n>] [--expect <hz>]"
                    ));
                }
            }
        }
        if options.seconds == 0 {
            return Err("--seconds must be at least 1".into());
        }
        Ok(options)
    }
}

fn main() -> ExitCode {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("capture-check: {message}");
            return ExitCode::from(2);
        }
    };
    match run(&options) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(message) => {
            eprintln!("capture-check: {message}");
            ExitCode::from(2)
        }
    }
}

/// What the capture loop collected.
#[derive(Default)]
struct Recording {
    left: Vec<f32>,
    right: Vec<f32>,
    packets: u32,
    /// Packets flagged `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`, the first one excepted (the
    /// engine flags the start of a stream).
    discontinuities: u32,
    silent_packets: u32,
    timeouts: u32,
}

fn run(options: &Options) -> Result<bool, String> {
    // SAFETY: first COM call of this thread; the thread stays in the MTA until exit.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .map_err(|e| format!("CoInitializeEx: {e}"))?;
    let (device, name) = find_endpoint(&options.name)?;
    println!("endpoint: {name}");

    // SAFETY: `device` is a live endpoint; no activation parameters.
    let client: IAudioClient =
        unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|e| format!("Activate: {e}"))?;
    // SAFETY: `client` is a live, uninitialized audio client.
    let format = unsafe { client.GetMixFormat() }.map_err(|e| format!("GetMixFormat: {e}"))?;
    let result = record(&client, format, options);
    // SAFETY: `GetMixFormat` allocated the format with `CoTaskMemAlloc`; not used afterwards.
    unsafe { CoTaskMemFree(Some(format.cast_const().cast())) };
    let (recording, rate) = result?;
    Ok(report(&recording, rate, options.expect))
}

/// The first active capture endpoint whose friendly name contains `needle` (any case).
fn find_endpoint(needle: &str) -> Result<(IMMDevice, String), String> {
    // SAFETY: COM is initialized on this thread.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| format!("MMDeviceEnumerator: {e}"))?;
    // SAFETY: `enumerator` is live.
    let devices = unsafe { enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) }
        .map_err(|e| format!("EnumAudioEndpoints: {e}"))?;
    // SAFETY: `devices` is live.
    let count = unsafe { devices.GetCount() }.map_err(|e| format!("GetCount: {e}"))?;
    let needle = needle.to_lowercase();
    let mut seen = String::new();
    for i in 0..count {
        // SAFETY: `i` is below the collection's count.
        let device = unsafe { devices.Item(i) }.map_err(|e| format!("Item: {e}"))?;
        let name = friendly_name(&device)?;
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

fn friendly_name(device: &IMMDevice) -> Result<String, String> {
    // SAFETY: `device` is live.
    let store =
        unsafe { device.OpenPropertyStore(STGM_READ) }.map_err(|e| format!("properties: {e}"))?;
    // SAFETY: `store` is live; the key is a valid constant.
    let value = unsafe { store.GetValue(&PKEY_Device_FriendlyName) }
        .map_err(|e| format!("friendly name: {e}"))?;
    // SAFETY: `value` is a valid PROPVARIANT owned here.
    let name = unsafe { PropVariantToBSTR(&raw const value) }
        .map_err(|e| format!("friendly name: {e}"))?;
    Ok(name.to_string())
}

/// Records `options.seconds` seconds; returns the recording and the frame rate.
fn record(
    client: &IAudioClient,
    format: *mut WAVEFORMATEX,
    options: &Options,
) -> Result<(Recording, u32), String> {
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
    println!(
        "mix format: {rate} Hz, {channels} channels, {bits}-bit {}",
        if float { "float" } else { "integer" }
    );
    if !float || bits != 32 || channels == 0 {
        return Err("expected a 32-bit float mix format (shared mode)".into());
    }
    let channels = usize::from(channels);

    // SAFETY: `client` is live and not initialized yet; `format` is its own mix format.
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
    // SAFETY: an auto-reset, unnamed event.
    let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
        .map_err(|e| format!("CreateEventW: {e}"))?;
    let result = capture(client, event, rate, channels, options.seconds);
    // SAFETY: `event` is a handle this function created; the client no longer signals it
    // (stopped in `capture`, or never started).
    let _ = unsafe { CloseHandle(event) };
    Ok((result?, rate))
}

fn capture(
    client: &IAudioClient,
    event: HANDLE,
    rate: u32,
    channels: usize,
    seconds: u32,
) -> Result<Recording, String> {
    // SAFETY: `client` is initialized for event callbacks; `event` is a valid event.
    unsafe { client.SetEventHandle(event) }.map_err(|e| format!("SetEventHandle: {e}"))?;
    // SAFETY: `client` is initialized.
    let capture: IAudioCaptureClient =
        unsafe { client.GetService() }.map_err(|e| format!("GetService: {e}"))?;

    let wanted = usize::try_from(u64::from(rate) * u64::from(seconds)).unwrap_or(usize::MAX);
    let mut recording = Recording {
        left: Vec::with_capacity(wanted + usize::try_from(rate).unwrap_or(0)),
        right: Vec::with_capacity(wanted + usize::try_from(rate).unwrap_or(0)),
        ..Recording::default()
    };
    let frame_bytes = channels * 4;

    // SAFETY: `client` is initialized and has its event.
    unsafe { client.Start() }.map_err(|e| format!("Start: {e}"))?;
    while recording.left.len() < wanted {
        // SAFETY: `event` is a valid event handle.
        if unsafe { WaitForSingleObject(event, 2_000) } != WAIT_OBJECT_0 {
            recording.timeouts += 1;
            if recording.timeouts >= 3 {
                break;
            }
            continue;
        }
        loop {
            // SAFETY: `capture` is live.
            let size = unsafe { capture.GetNextPacketSize() }
                .map_err(|e| format!("GetNextPacketSize: {e}"))?;
            if size == 0 {
                break;
            }
            let mut data = ptr::null_mut();
            let mut frames = 0;
            let mut flags = 0;
            // SAFETY: valid out-parameters; positions not requested.
            unsafe {
                capture.GetBuffer(&raw mut data, &raw mut frames, &raw mut flags, None, None)
            }
            .map_err(|e| format!("GetBuffer: {e}"))?;
            recording.packets += 1;
            if flags & flag(AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0) != 0 && recording.packets > 1
            {
                recording.discontinuities += 1;
            }
            let count = usize::try_from(frames).unwrap_or(0);
            if flags & flag(AUDCLNT_BUFFERFLAGS_SILENT.0) != 0 || data.is_null() {
                recording.silent_packets += 1;
                recording.left.extend(std::iter::repeat_n(0.0, count));
                recording.right.extend(std::iter::repeat_n(0.0, count));
            } else {
                // SAFETY: `GetBuffer` returned `frames` frames of `frame_bytes` bytes at
                // `data`, valid until `ReleaseBuffer`.
                let bytes = unsafe { slice::from_raw_parts(data, count * frame_bytes) };
                for frame in bytes.chunks_exact(frame_bytes) {
                    let sample = |c: usize| {
                        f32::from_le_bytes([
                            frame[4 * c],
                            frame[4 * c + 1],
                            frame[4 * c + 2],
                            frame[4 * c + 3],
                        ])
                    };
                    recording.left.push(sample(0));
                    recording.right.push(sample(usize::from(channels > 1)));
                }
            }
            // SAFETY: releases exactly the packet obtained above.
            unsafe { capture.ReleaseBuffer(frames) }.map_err(|e| format!("ReleaseBuffer: {e}"))?;
        }
    }
    // SAFETY: `client` was started above.
    unsafe { client.Stop() }.map_err(|e| format!("Stop: {e}"))?;
    Ok(recording)
}

/// A WASAPI buffer flag as the `u32` that `GetBuffer` returns.
fn flag(value: i32) -> u32 {
    value.cast_unsigned()
}

/// Prints the measurements and returns whether the recording passes.
fn report(recording: &Recording, rate: u32, expect: f64) -> bool {
    let skip = usize::try_from(f64_to_u64(SETTLE_SECONDS * f64::from(rate))).unwrap_or(0);
    let left = recording.left.get(skip..).unwrap_or(&[]);
    let right = recording.right.get(skip..).unwrap_or(&[]);
    let analysis = analysis::analyse(left, rate);
    let channel_difference = left
        .iter()
        .zip(right)
        .map(|(l, r)| (l - r).abs())
        .fold(0.0_f32, f32::max);

    println!(
        "recorded: {} frames ({:.2} s) in {} packets",
        recording.left.len(),
        seconds(recording.left.len(), rate),
        recording.packets
    );
    println!(
        "engine: {} discontinuity flags (after the first packet), {} silent packets, {} timeouts",
        recording.discontinuities, recording.silent_packets, recording.timeouts
    );
    match analysis.frequency {
        Some(f) => println!("frequency: {f:.2} Hz (expected {expect} Hz)"),
        None => println!("frequency: none (no zero crossings)"),
    }
    println!(
        "level: peak {:.2} dBFS, RMS {:.2} dBFS; max |left - right| {channel_difference:.6}",
        analysis.peak_dbfs, analysis.rms_dbfs
    );
    match analysis.first_glitch {
        Some(first) => println!(
            "glitches: {} (first at {:.3} s)",
            analysis.glitches,
            seconds(first + skip, rate)
        ),
        None => println!("glitches: 0"),
    }

    let passed = analysis
        .frequency
        .is_some_and(|f| (f - expect).abs() <= 1.0)
        && analysis.glitches == 0
        && analysis.peak_dbfs > -60.0
        && recording.discontinuities == 0
        && recording.silent_packets == 0
        && recording.timeouts == 0;
    println!("{}", if passed { "PASS" } else { "FAIL" });
    passed
}

#[allow(
    clippy::cast_precision_loss,
    reason = "frame counts are far below 2^52"
)]
fn seconds(frames: usize, rate: u32) -> f64 {
    frames as f64 / f64::from(rate)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "small positive durations"
)]
fn f64_to_u64(value: f64) -> u64 {
    value.max(0.0) as u64
}
