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
//! The driver maps its shared buffer into the host on attach. While the stream runs, the host
//! prints once a second the peak of what the stream's part of the buffer holds: until the host
//! advances the position (a later pull request, PLAN.md, M4), the driver writes its test tone
//! there, so the peak shows that the host sees the audio engine's pages. Exit status: 0 on
//! success, 1 on failure, 2 on a usage error.

use std::{
    env,
    process::ExitCode,
    time::{Duration, Instant},
};

use hostmic_abi::{
    ABI_VERSION, AttachReply, AttachRequest, FormatRange, GUID_DEVINTERFACE_HOSTMIC_HOST,
    IOCTL_ATTACH, IOCTL_DETACH, IOCTL_GET_STREAM, SampleType, State, StreamState,
};
use windows::{
    Win32::{
        Devices::DeviceAndDriverInstallation::{
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
            CM_Get_Device_Interface_ListW, CR_SUCCESS,
        },
        Foundation::{
            CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, HANDLE_FLAG_INHERIT,
            SetHandleInformation, WAIT_OBJECT_0,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING,
        },
        System::{
            IO::DeviceIoControl,
            Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery},
            Threading::{
                CREATE_NO_WINDOW, CreateEventW, CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW,
                WaitForSingleObject,
            },
        },
    },
    core::{GUID, HSTRING, PCWSTR, PWSTR},
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

    let end = Instant::now() + Duration::from_secs(options.seconds);
    let mut stream = StreamState::default();
    let mut next_peak = Instant::now();
    while Instant::now() < end {
        // SAFETY: an event handle this process owns.
        let wait = unsafe { WaitForSingleObject(event.0, 250) };
        if wait == WAIT_OBJECT_0 {
            let mut state = [0u8; StreamState::SIZE];
            device
                .control(IOCTL_GET_STREAM, &[], &mut state)
                .map_err(|e| format!("GET_STREAM: {e}"))?;
            stream = StreamState::from_bytes(&state);
            print_state(&stream);
        }
        if State::from_code(stream.state) == Some(State::Running) && Instant::now() >= next_peak {
            next_peak = Instant::now() + Duration::from_secs(1);
            // SAFETY: the driver mapped `buffer_bytes` bytes at `buffer` into this process
            // until DETACH, which comes after the loop.
            match unsafe { peak(&reply, &stream) } {
                Some(peak) => println!(
                    "buffer     peak {} over {} frames",
                    dbfs(peak),
                    stream.buffer_frames
                ),
                None => println!("buffer     the stream's buffer does not fit the mapping"),
            }
        }
    }

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

/// The largest absolute 16-bit sample in the stream's part of the shared buffer, or `None` when
/// the stream's format is not 16-bit or its buffer exceeds the mapping.
///
/// # Safety
///
/// `reply` describes a live mapping of the shared buffer in this process.
unsafe fn peak(reply: &AttachReply, stream: &StreamState) -> Option<u16> {
    if SampleType::from_code(stream.format.sample_type) != Some(SampleType::Int16) {
        return None;
    }
    let samples = usize::try_from(stream.buffer_frames)
        .ok()?
        .checked_mul(usize::from(stream.format.channels))?;
    if samples.checked_mul(2)? > reply.buffer_bytes as usize {
        return None;
    }
    let base = usize::try_from(reply.buffer).ok()? as *const i16;
    let mut peak = 0u16;
    for index in 0..samples {
        // Within the mapping (checked above), 2-byte aligned (page-aligned base).
        let sample = base.wrapping_add(index);
        // SAFETY: as above. The driver writes the same pages concurrently: a volatile read of
        // each sample, which may be old or new, never torn (an aligned 16-bit read).
        let sample = unsafe { sample.read_volatile() };
        peak = peak.max(sample.unsigned_abs());
    }
    Some(peak)
}

/// A 16-bit peak in dBFS, two decimals.
fn dbfs(peak: u16) -> String {
    if peak == 0 {
        return "-inf dBFS (silence)".to_owned();
    }
    format!("{:.2} dBFS", 20.0 * (f64::from(peak) / 32768.0).log10())
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
        let mut returned = 0u32;
        let input_length = u32::try_from(input.len()).unwrap_or(u32::MAX);
        let output_length = u32::try_from(output.len()).unwrap_or(u32::MAX);
        // SAFETY: an open handle; buffers of the given lengths that live through the
        // synchronous call; a writable byte count.
        unsafe {
            DeviceIoControl(
                self.0,
                code,
                (!input.is_empty()).then_some(input.as_ptr().cast()),
                input_length,
                (!output.is_empty()).then_some(output.as_mut_ptr().cast()),
                output_length,
                Some(&raw mut returned),
                None,
            )
        }
        .map_err(|e| e.to_string())?;
        if returned != output_length {
            return Err(format!(
                "{returned} bytes returned, {output_length} expected"
            ));
        }
        Ok(())
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: a handle this value owns, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}
