//! `test-host`: a test host for the hostmic driver.
//!
//! ```text
//! test-host [--seconds N] [--no-detach] [--holder [--close-own-handle]]
//! ```
//!
//! Attaches to the first host interface, then prints the capture stream's state each time the
//! driver signals a change, for `N` seconds (default 30), and detaches. With `--no-detach` it
//! exits without detaching, to check that closing the handle detaches the host.
//!
//! Two options check that the host's mapping never outlives it, whoever holds its handle:
//! `--holder` starts a process that inherits the host's handle (`ping -n 3600 127.0.0.1`) and
//! prints its id, then goes on as usual (kill the host: the driver must undo the mapping while
//! the holder keeps the handle open); `--close-own-handle` also closes the host's own handle,
//! so that the holder's is the last one, then watches its mapping without touching it (kill the
//! holder: the driver must undo the mapping in this process, from the holder's).
//!
//! While the stream runs, the host plays a 1 kHz tone at -6.02 dBFS: it writes the frames due
//! by the performance counter into the shared buffer the driver mapped on attach, and advances
//! the stream position (`ADVANCE`) about once a millisecond, from a thread registered with
//! MMCSS. The driver produces no audio and keeps no clock of its own: what
//! `tools/capture-check` records is what this host wrote, at its pace. At the end it prints
//! the driver's counters (`GET_STATS`). Exit status: 0 on success, 1 on failure, 2 on a usage
//! error.

use std::{
    env,
    process::ExitCode,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use hostmic_abi::{
    ABI_VERSION, AdvanceReply, AdvanceRequest, AttachReply, AttachRequest, FormatRange,
    GUID_DEVINTERFACE_HOSTMIC_HOST, IOCTL_ADVANCE, IOCTL_ATTACH, IOCTL_DETACH, IOCTL_GET_STATS,
    IOCTL_GET_STREAM, SampleType, State, Stats, StreamState,
};
use windows::{
    Win32::{
        Devices::DeviceAndDriverInstallation::{
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
            CM_Get_Device_Interface_ListW, CR_SUCCESS,
        },
        Foundation::{
            CloseHandle, ERROR_NOT_READY, GENERIC_READ, GENERIC_WRITE, HANDLE, HANDLE_FLAG_INHERIT,
            SetHandleInformation, WAIT_OBJECT_0,
        },
        Media::{timeBeginPeriod, timeEndPeriod},
        Storage::FileSystem::{
            CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING,
        },
        System::{
            IO::DeviceIoControl,
            Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery},
            Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
            Threading::{
                AvSetMmThreadCharacteristicsW, CREATE_NO_WINDOW, CreateEventW, CreateProcessW,
                PROCESS_INFORMATION, STARTUPINFOW, WaitForSingleObject,
            },
        },
    },
    core::{GUID, HSTRING, PCWSTR, PWSTR, w},
};

const USAGE: &str = "usage: test-host [--seconds N] [--no-detach] [--holder [--close-own-handle]]";

/// Command-line options.
struct Options {
    seconds: u64,
    detach: bool,
    holder: bool,
    close_own_handle: bool,
}

fn parse() -> Result<Options, String> {
    let mut options = Options {
        seconds: 30,
        detach: true,
        holder: false,
        close_own_handle: false,
    };
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seconds" => {
                let value = args.next().ok_or("--seconds needs a value")?;
                options.seconds = value
                    .parse()
                    .map_err(|_| format!("--seconds: not a number: {value}"))?;
            }
            "--no-detach" => options.detach = false,
            "--holder" => options.holder = true,
            "--close-own-handle" => options.close_own_handle = true,
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }
    if options.close_own_handle && !options.holder {
        return Err("--close-own-handle needs --holder".to_owned());
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

fn run(options: &Options) -> Result<(), String> {
    let path = interface_paths()?
        .into_iter()
        .next()
        .ok_or("no hostmic host interface: is the driver installed and started?")?;
    println!("interface  {path}");
    let device = Handle::open(&path).map_err(|e| format!("open: {e}"))?;
    // SAFETY: no security attributes; an unnamed auto-reset event, initially not signaled.
    let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
        .map(Handle)
        .map_err(|e| format!("CreateEventW: {e}"))?;

    let request = AttachRequest {
        abi_version: ABI_VERSION,
        reserved: 0,
        // Handle values fit in 32 bits; zero-extended, as the ABI says.
        event: event.0.0 as usize as u64,
        formats: FormatRange {
            min_rate: 48_000,
            max_rate: 48_000,
            min_channels: 2,
            max_channels: 2,
            sample_types: SampleType::Int16.bit(),
        },
    };
    let mut reply = [0u8; AttachReply::SIZE];
    device
        .control(IOCTL_ATTACH, &request.to_bytes(), &mut reply)
        .map_err(|e| format!("ATTACH: {e}"))?;
    let reply = AttachReply::from_bytes(&reply);
    println!(
        "attached   buffer {:#x}, {} bytes",
        reply.buffer, reply.buffer_bytes
    );

    if reply.buffer == 0 || !reply.buffer.is_multiple_of(4096) {
        return Err(format!(
            "ATTACH: unusable buffer address {:#x}",
            reply.buffer
        ));
    }
    if options.holder {
        let holder = spawn_holder(&device)?;
        println!("holder     pid {holder} inherited the host's handle");
        if options.close_own_handle {
            drop(device);
            println!("host       closed its own handle: the holder's is the last");
            watch_mapping(&reply, options.seconds);
            return Ok(());
        }
    }

    // SAFETY: the driver mapped `buffer_bytes` bytes at `buffer` into this process until
    // DETACH or the handle's close, which come after `play` returns.
    unsafe { play(&device, &event, &reply, options.seconds) }?;

    let mut stats = [0u8; Stats::SIZE];
    device
        .control(IOCTL_GET_STATS, &[], &mut stats)
        .map_err(|e| format!("GET_STATS: {e}"))?;
    let stats = Stats::from_bytes(&stats);
    println!(
        "driver     {} frames advanced, {} packets completed, {} skipped, {} requests refused",
        stats.frames_advanced,
        stats.packets_completed,
        stats.packets_skipped,
        stats.requests_refused
    );

    if options.detach {
        device
            .control(IOCTL_DETACH, &[], &mut [])
            .map_err(|e| format!("DETACH: {e}"))?;
        println!("detached");
    } else {
        println!("exiting without DETACH: closing the handle detaches");
    }
    Ok(())
}

/// Starts a process that inherits `device`'s handle and lives for an hour; returns its id.
fn spawn_holder(device: &Handle) -> Result<u32, String> {
    // SAFETY: an open handle this process owns.
    unsafe { SetHandleInformation(device.0, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) }
        .map_err(|e| format!("SetHandleInformation: {e}"))?;
    let mut command: Vec<u16> = "ping.exe -n 3600 127.0.0.1\0".encode_utf16().collect();
    let startup = STARTUPINFOW {
        cb: u32::try_from(size_of::<STARTUPINFOW>()).unwrap_or(0),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: a writable NUL-terminated command line; startup and process information valid
    // for the call; handles inherited on purpose.
    unsafe {
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &raw const startup,
            &raw mut process,
        )
    }
    .map_err(|e| format!("CreateProcessW: {e}"))?;
    // The holder runs on its own; its handles here are not needed.
    drop(Handle(process.hThread));
    drop(Handle(process.hProcess));
    Ok(process.dwProcessId)
}

/// Prints whether the shared buffer is still mapped at the address `ATTACH` returned, each time
/// that changes, for `seconds` seconds. Only queries the address, never reads it.
fn watch_mapping(reply: &AttachReply, seconds: u64) {
    let Ok(address) = usize::try_from(reply.buffer) else {
        return;
    };
    let end = Instant::now() + Duration::from_secs(seconds);
    let mut last = None;
    while Instant::now() < end {
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: queries this process's address space; `info` is writable.
        let written = unsafe {
            VirtualQuery(
                Some(address as *const core::ffi::c_void),
                &raw mut info,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        let mapped = written != 0 && info.State == MEM_COMMIT;
        if last != Some(mapped) {
            println!(
                "mapping    {:#x}: {}",
                reply.buffer,
                if mapped { "mapped" } else { "unmapped" }
            );
            last = Some(mapped);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Frames per second of the one format the driver streams.
const RATE: u64 = 48_000;

/// One period of the test tone: 1 kHz at 48 kHz is exactly 48 frames, at half of full scale
/// (-6.02 dBFS), the same signal `tools/capture-check` expects.
fn tone_period() -> [i16; 48] {
    core::array::from_fn(|n| {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            reason = "|16384 sin x| <= 16384 fits an i16; n < 48 is exact"
        )]
        let sample = (16_384.0 * (core::f64::consts::TAU * n as f64 / 48.0).sin()).round() as i16;
        sample
    })
}

/// Writes the tone for the stream frames `frames` into the ring of `frames_in_buffer` 16-bit
/// stereo frames at `base`, both channels alike.
///
/// # Safety
///
/// `base` is a live, page-aligned mapping of at least `frames_in_buffer` frames (4 bytes each).
unsafe fn write_tone(
    base: *mut u32,
    tone: &[i16; 48],
    frames: core::ops::Range<u64>,
    frames_in_buffer: u64,
) {
    for frame in frames {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "remainders below 48 and below the buffer's frame count"
        )]
        let (sample, index) = (
            tone[(frame % 48) as usize].cast_unsigned(),
            (frame % frames_in_buffer) as usize,
        );
        // SAFETY: `index` is below `frames_in_buffer`, within the mapping, 4-byte aligned
        // (page-aligned base). The audio engine reads these pages concurrently through its own
        // mapping: a volatile write of whole frames.
        unsafe {
            base.wrapping_add(index)
                .write_volatile(u32::from(sample) | (u32::from(sample) << 16));
        };
    }
}

/// Performance counter ticks in milliseconds.
fn ms(ticks: i64, frequency: i64) -> f64 {
    #[allow(
        clippy::cast_precision_loss,
        reason = "a duration for display, far below 2^52 ticks"
    )]
    let ms = ticks as f64 * 1_000.0 / frequency.max(1) as f64;
    ms
}

/// The performance counter.
fn qpc() -> i64 {
    let mut now = 0;
    // SAFETY: a writable counter; cannot fail on Windows XP and later.
    let _ = unsafe { QueryPerformanceCounter(&raw mut now) };
    now
}

/// What the player thread tells the main thread, which prints it: the player itself never
/// writes to the console or a file, which can block for tens of milliseconds.
enum Note {
    /// The player thread's scheduling (MMCSS).
    Thread(String),
    /// The stream changed.
    Stream(StreamState),
    /// More than a buffer of frames was due at `position`: the host was not scheduled for
    /// `gap_ms` milliseconds.
    Late {
        position: u64,
        frames: u64,
        gap_ms: f64,
    },
    /// Once a second while playing.
    Progress {
        position: u64,
        advanced: u64,
        refused: u64,
        late: u64,
        longest_gap_ms: f64,
    },
}

/// Plays the test tone into the stream for `seconds` seconds, as a host does: a player thread
/// registered with MMCSS ("Pro Audio") writes, while the stream runs, the frames due by the
/// performance counter into the shared buffer and advances the position about once a
/// millisecond; this thread prints what it reports.
///
/// # Safety
///
/// `reply` describes a live mapping of the shared buffer in this process for the whole call.
unsafe fn play(
    device: &Handle,
    event: &Handle,
    reply: &AttachReply,
    seconds: u64,
) -> Result<(), String> {
    let mut frequency = 0;
    // SAFETY: a writable frequency.
    unsafe { QueryPerformanceFrequency(&raw mut frequency) }
        .map_err(|e| format!("QueryPerformanceFrequency: {e}"))?;
    let base = Mapping(usize::try_from(reply.buffer).map_err(|_| "buffer address")? as *mut u32);
    let buffer_bytes = u64::from(reply.buffer_bytes);
    let (notes, received) = mpsc::channel();
    thread::scope(|scope| {
        let player = scope.spawn(move || {
            let mut player = Player {
                device,
                base,
                buffer_bytes,
                frequency,
                notes,
                tone: tone_period(),
            };
            player.run(event, Duration::from_secs(seconds))
        });
        for note in received {
            print_note(&note);
        }
        player
            .join()
            .map_err(|_| "the player thread panicked".to_owned())?
    })
}

fn print_note(note: &Note) {
    match note {
        Note::Thread(text) => println!("thread     {text}"),
        Note::Stream(stream) => print_state(stream),
        Note::Late {
            position,
            frames,
            gap_ms,
        } => println!(
            "late       at position {position}: {frames} frames due, {gap_ms:.1} ms since the previous write"
        ),
        Note::Progress {
            position,
            advanced,
            refused,
            late,
            longest_gap_ms,
        } => println!(
            "playing    position {position}  {advanced} frames advanced  {refused} refused  {late} late  longest gap {longest_gap_ms:.1} ms"
        ),
    }
}

/// The shared buffer's address in this process.
#[derive(Clone, Copy)]
struct Mapping(*mut u32);

// SAFETY: an address in this process's space, valid for every thread; `play`'s contract keeps
// the mapping alive while the player uses it.
unsafe impl Send for Mapping {}

/// The player thread's state.
struct Player<'a> {
    device: &'a Handle,
    base: Mapping,
    buffer_bytes: u64,
    frequency: i64,
    notes: mpsc::Sender<Note>,
    tone: [i16; 48],
}

impl Player<'_> {
    /// The player loop, for `duration`.
    fn run(&mut self, event: &Handle, duration: Duration) -> Result<(), String> {
        let mut task = 0u32;
        // SAFETY: a static task name and a writable task index. Failure only loses priority.
        let mmcss = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &raw mut task) };
        let _ = self.notes.send(Note::Thread(match mmcss {
            Ok(_) => "MMCSS Pro Audio".to_owned(),
            Err(error) => format!("no MMCSS: {error}"),
        }));
        // SAFETY: no precondition; undone below.
        unsafe { timeBeginPeriod(1) };
        let result = self.play_for(event, duration);
        // SAFETY: matches `timeBeginPeriod` above.
        unsafe { timeEndPeriod(1) };
        result
    }

    fn play_for(&mut self, event: &Handle, duration: Duration) -> Result<(), String> {
        let end = Instant::now() + duration;
        let mut next_report = Instant::now();
        let mut stream = StreamState::default();
        // Performance counter and position at which the current run's clock starts.
        let mut anchor: Option<(i64, u64)> = None;
        let mut position = 0u64;
        let (mut advanced, mut refused, mut late) = (0u64, 0u64, 0u64);
        // The performance counter at the last write, and the longest time between two writes.
        let mut last_write: Option<i64> = None;
        let mut longest_gap = 0i64;
        let mut refresh = false;
        while Instant::now() < end {
            // SAFETY: an event handle this process owns.
            let wait = unsafe { WaitForSingleObject(event.0, 1) };
            if wait == WAIT_OBJECT_0 || refresh {
                refresh = false;
                let previous = stream;
                stream = self.stream()?;
                if stream != previous {
                    let _ = self.notes.send(Note::Stream(stream));
                }
                // Running for the host: the running state under an even (accepting) generation,
                // with a buffer. The driver may report `running` one step early, under the
                // last odd generation.
                let running = State::from_code(stream.state) == Some(State::Running)
                    && stream.generation % 2 == 0
                    && stream.buffer_frames != 0;
                if !running {
                    anchor = None;
                    last_write = None;
                } else if anchor.is_none() || stream.generation != previous.generation {
                    anchor = Some((qpc(), stream.position));
                    position = stream.position;
                    last_write = None;
                }
            }
            let Some((start, from)) = anchor else {
                continue;
            };
            let frames_in_buffer = u64::from(stream.buffer_frames);
            // Not while `running` requires a buffer; kept against a division by zero below.
            if frames_in_buffer == 0 {
                anchor = None;
                continue;
            }
            if frames_in_buffer * 4 > self.buffer_bytes {
                return Err(format!(
                    "a buffer of {frames_in_buffer} frames does not fit the mapping"
                ));
            }
            let now = qpc();
            let due = from + self.frames_in(now - start);
            if due <= position {
                continue;
            }
            let mut frames = due - position;
            let gap = now - last_write.unwrap_or(now);
            last_write = Some(now);
            longest_gap = longest_gap.max(gap);
            if frames > frames_in_buffer {
                // More than a buffer late: the engine lost those frames anyway; write one
                // buffer and restart the clock from there.
                late += 1;
                let _ = self.notes.send(Note::Late {
                    position,
                    frames,
                    gap_ms: ms(gap, self.frequency),
                });
                frames = frames_in_buffer;
                anchor = Some((now, position + frames));
            }
            // SAFETY: the stream's frames fit the mapping (checked above).
            unsafe {
                write_tone(
                    self.base.0,
                    &self.tone,
                    position..position + frames,
                    frames_in_buffer,
                );
            };
            if let Some(next) = self.advance(stream.generation, frames, position)? {
                position = next;
                advanced += frames;
            } else {
                // The stream changed meanwhile: read its state again.
                refused += 1;
                anchor = None;
                refresh = true;
            }
            if Instant::now() >= next_report {
                next_report = Instant::now() + Duration::from_secs(1);
                let _ = self.notes.send(Note::Progress {
                    position,
                    advanced,
                    refused,
                    late,
                    longest_gap_ms: ms(longest_gap, self.frequency),
                });
            }
        }
        Ok(())
    }

    /// `GET_STREAM`.
    fn stream(&self) -> Result<StreamState, String> {
        let mut state = [0u8; StreamState::SIZE];
        self.device
            .control(IOCTL_GET_STREAM, &[], &mut state)
            .map_err(|e| format!("GET_STREAM: {e}"))?;
        Ok(StreamState::from_bytes(&state))
    }

    /// `ADVANCE` by `frames` from `from`: the new position, or `None` when the request no longer
    /// matches the stream (a state change raced it: `ERROR_NOT_READY`, see `IOCTL_ADVANCE`).
    ///
    /// # Errors
    ///
    /// Any other refusal: this process is no longer the attached host, for instance.
    fn advance(&self, generation: u32, frames: u64, from: u64) -> Result<Option<u64>, String> {
        let request = AdvanceRequest {
            generation,
            frames: u32::try_from(frames).map_err(|_| "too many frames")?,
            from,
        };
        let mut answer = [0u8; AdvanceReply::SIZE];
        match self
            .device
            .call(IOCTL_ADVANCE, &request.to_bytes(), &mut answer)
        {
            Ok(returned) if returned as usize == AdvanceReply::SIZE => {
                Ok(Some(AdvanceReply::from_bytes(&answer).position))
            }
            Ok(returned) => Err(format!("ADVANCE returned {returned} bytes")),
            Err(error) if error.code() == ERROR_NOT_READY.to_hresult() => Ok(None),
            Err(error) => Err(format!("ADVANCE: {error}")),
        }
    }

    /// Frames in `ticks` performance counter ticks, rounded down; 0 for a negative count.
    fn frames_in(&self, ticks: i64) -> u64 {
        let ticks = u128::from(u64::try_from(ticks).unwrap_or(0));
        u64::try_from(ticks * u128::from(RATE) / u128::from(self.frequency.unsigned_abs().max(1)))
            .unwrap_or(u64::MAX)
    }
}

/// One line per state, with units.
fn print_state(state: &StreamState) {
    let name = match State::from_code(state.state) {
        Some(State::Stopped) => "stopped",
        Some(State::Paused) => "paused",
        Some(State::Running) => "running",
        None => "unknown",
    };
    let format = state.format;
    if format.rate == 0 {
        println!(
            "stream     gen {:>5}  {name:<8} no stream",
            state.generation
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
        "stream     gen {:>5}  {:<8} {} Hz {} ch {}  buffer {} frames  packet {} frames  position {}",
        state.generation,
        name,
        format.rate,
        format.channels,
        sample,
        state.buffer_frames,
        state.frames_per_packet,
        state.position
    );
}

/// The paths of the present host interfaces.
fn interface_paths() -> Result<Vec<String>, String> {
    let guid = GUID::from_values(
        GUID_DEVINTERFACE_HOSTMIC_HOST.data1,
        GUID_DEVINTERFACE_HOSTMIC_HOST.data2,
        GUID_DEVINTERFACE_HOSTMIC_HOST.data3,
        GUID_DEVINTERFACE_HOSTMIC_HOST.data4,
    );
    for _ in 0..4 {
        let mut length = 0u32;
        // SAFETY: a writable length and a valid GUID; no device instance filter.
        let status = unsafe {
            CM_Get_Device_Interface_List_SizeW(
                &raw mut length,
                &raw const guid,
                PCWSTR::null(),
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if status != CR_SUCCESS {
            return Err(format!(
                "CM_Get_Device_Interface_List_SizeW: CONFIGRET {}",
                status.0
            ));
        }
        let mut buffer = vec![0u16; length as usize];
        // SAFETY: a valid GUID and a buffer of the size just returned.
        let status = unsafe {
            CM_Get_Device_Interface_ListW(
                &raw const guid,
                PCWSTR::null(),
                &mut buffer,
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if status == CR_SUCCESS {
            return Ok(buffer
                .split(|&unit| unit == 0)
                .filter(|path| !path.is_empty())
                .map(String::from_utf16_lossy)
                .collect());
        }
    }
    Err("the interface list kept changing".to_owned())
}

/// A handle this process owns, closed on drop.
struct Handle(HANDLE);

// SAFETY: a kernel handle is valid in every thread of the process, and the calls made with it
// (`DeviceIoControl`, `WaitForSingleObject`) may run concurrently from several threads; it is
// closed only when the `Handle` is dropped, after the threads that borrow it end.
unsafe impl Sync for Handle {}

impl Handle {
    /// Opens `path` for reading and writing, as a host must.
    fn open(path: &str) -> Result<Self, windows::core::Error> {
        let path = HSTRING::from(path);
        // SAFETY: a NUL-terminated path; no security attributes or template.
        let handle = unsafe {
            CreateFileW(
                &path,
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }?;
        Ok(Self(handle))
    }

    /// A synchronous device control request; the reply must fill `output` exactly.
    fn control(&self, code: u32, input: &[u8], output: &mut [u8]) -> Result<(), String> {
        let output_length = u32::try_from(output.len()).unwrap_or(u32::MAX);
        let returned = self.call(code, input, output).map_err(|e| e.to_string())?;
        if returned != output_length {
            return Err(format!(
                "{returned} bytes returned, {output_length} expected"
            ));
        }
        Ok(())
    }

    /// `DeviceIoControl`, synchronous; the bytes returned.
    fn call(&self, code: u32, input: &[u8], output: &mut [u8]) -> windows::core::Result<u32> {
        let mut returned = 0u32;
        // SAFETY: an open handle; buffers of the given lengths that live through the
        // synchronous call; a writable byte count.
        unsafe {
            DeviceIoControl(
                self.0,
                code,
                (!input.is_empty()).then_some(input.as_ptr().cast()),
                u32::try_from(input.len()).unwrap_or(u32::MAX),
                (!output.is_empty()).then_some(output.as_mut_ptr().cast()),
                u32::try_from(output.len()).unwrap_or(u32::MAX),
                Some(&raw mut returned),
                None,
            )
        }?;
        Ok(returned)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: a handle this value owns, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}
