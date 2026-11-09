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

use hostmic_abi::{
    GUID_DEVINTERFACE_HOSTMIC_HOST, IOCTL_ADVANCE, IOCTL_ATTACH, IOCTL_DETACH, IOCTL_GET_INFO,
    IOCTL_GET_STATS, IOCTL_GET_STREAM, Info, Reason, SampleType, State, Stats, StreamState,
    function,
};
use windows::{
    Win32::{
        Devices::DeviceAndDriverInstallation::{
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
            CM_Get_Device_Interface_ListW, CR_SUCCESS,
        },
        Foundation::{CloseHandle, GENERIC_READ, HANDLE},
        Storage::FileSystem::{
            CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING,
        },
        System::IO::DeviceIoControl,
    },
    core::{GUID, HSTRING, PCWSTR},
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
    let paths = match interface_paths() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("cannot list host interfaces: {error}");
            return ExitCode::FAILURE;
        }
    };
    if paths.is_empty() {
        eprintln!("no hostmic host interface: is the driver installed and started?");
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
    let paths = match interface_paths() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("cannot list host interfaces: {error}");
            return ExitCode::FAILURE;
        }
    };
    if paths.is_empty() {
        eprintln!("no hostmic host interface: is the driver installed and started?");
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
        println!(
            "  last refusal  {}: {reason}, status {:#010x}",
            request_name(stats.last_request),
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

/// The paths of the present host interfaces.
fn interface_paths() -> Result<Vec<String>, String> {
    let guid = GUID::from_values(
        GUID_DEVINTERFACE_HOSTMIC_HOST.data1,
        GUID_DEVINTERFACE_HOSTMIC_HOST.data2,
        GUID_DEVINTERFACE_HOSTMIC_HOST.data3,
        GUID_DEVINTERFACE_HOSTMIC_HOST.data4,
    );
    // The list may grow between the two calls: retry a few times, as Microsoft's samples do.
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

/// An open host interface.
struct Device(HANDLE);

impl Device {
    /// Opens `path` for reading: enough for the requests that only read.
    fn open(path: &str) -> Result<Self, windows::core::Error> {
        let path = HSTRING::from(path);
        // SAFETY: a NUL-terminated path; no security attributes or template.
        let handle = unsafe {
            CreateFileW(
                &path,
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }?;
        Ok(Self(handle))
    }

    /// A request without input whose reply is exactly `N` bytes.
    fn read<const N: usize>(&self, code: u32) -> Result<[u8; N], windows::core::Error> {
        let mut reply = [0u8; N];
        let mut returned = 0u32;
        // SAFETY: an open handle; no input; an output buffer of `N` bytes that lives through
        // the synchronous call; a writable byte count.
        unsafe {
            DeviceIoControl(
                self.0,
                code,
                None,
                0,
                Some(reply.as_mut_ptr().cast()),
                u32::try_from(N).unwrap_or(u32::MAX),
                Some(&raw mut returned),
                None,
            )
        }?;
        if returned as usize != N {
            return Err(windows::core::Error::new(
                windows::core::HRESULT(0x8000_FFFF_u32.cast_signed()),
                format!(
                    "{} returned {returned} bytes, expected {N}",
                    request_name(function(code))
                ),
            ));
        }
        Ok(reply)
    }

    /// `IOCTL_GET_INFO`.
    fn info(&self) -> Result<Info, windows::core::Error> {
        self.read(IOCTL_GET_INFO)
            .map(|bytes| Info::from_bytes(&bytes))
    }

    /// `IOCTL_GET_STREAM`.
    fn stream(&self) -> Result<StreamState, windows::core::Error> {
        self.read(IOCTL_GET_STREAM)
            .map(|bytes| StreamState::from_bytes(&bytes))
    }

    /// `IOCTL_GET_STATS`.
    fn stats(&self) -> Result<Stats, windows::core::Error> {
        self.read(IOCTL_GET_STATS)
            .map(|bytes| Stats::from_bytes(&bytes))
    }
}

/// The name of a request by its control code's function number.
fn request_name(function_number: u16) -> String {
    [
        IOCTL_GET_INFO,
        IOCTL_ATTACH,
        IOCTL_GET_STREAM,
        IOCTL_ADVANCE,
        IOCTL_DETACH,
        IOCTL_GET_STATS,
    ]
    .into_iter()
    .zip([
        "GET_INFO",
        "ATTACH",
        "GET_STREAM",
        "ADVANCE",
        "DETACH",
        "GET_STATS",
    ])
    .find(|(code, _)| function(*code) == function_number)
    .map_or_else(
        || format!("function {function_number:#x}"),
        |(_, name)| name.to_owned(),
    )
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: a handle this value owns, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}
