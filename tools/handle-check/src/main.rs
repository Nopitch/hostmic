//! `handle-check`: checks the hostmic driver's handle rules, with raw requests rather than the
//! `hostmic` crate (whose `Host` always detaches on drop).
//!
//! ```text
//! handle-check [--seconds N] (--close | --holder [--close-own-handle] | --detach-from-child)
//! ```
//!
//! Attaches to the first host interface as its host, does what the option says, then watches
//! whether the shared buffer is still mapped for `N` seconds (default 30), printing every
//! change, and detaches if it still can.
//!
//! Closing the host's handle detaches it: `--close` closes this program's only handle, without
//! `DETACH`, while the program lives; the driver must undo the mapping at the handle's cleanup.
//!
//! The host's mapping must never outlive it, whoever holds its handle: `--holder` starts a
//! process that inherits the host's handle (`ping -n 3600 127.0.0.1`) and prints its id (kill
//! this program: the driver must undo the mapping while the holder keeps the handle open);
//! `--close-own-handle` also closes this program's own handle, so that the holder's is the last
//! one (kill the holder: the driver must undo the mapping in this process, from the holder's).
//!
//! And the mapping must last while the host lives: `--detach-from-child` starts a copy of this
//! program that inherits the host's handle and sends `DETACH` with it (`--send-detach`,
//! internal). The driver must refuse it (only the host's own process may detach); the mapping
//! then stays, and this program's own `DETACH` succeeds.
//!
//! Exit status: 0 on success, 1 on failure, 2 on a usage error.

use std::{
    env,
    process::ExitCode,
    time::{Duration, Instant},
};

use hostmic_abi::{
    ABI_VERSION, AttachReply, AttachRequest, FormatRange, GUID_DEVINTERFACE_HOSTMIC_HOST,
    IOCTL_ATTACH, IOCTL_DETACH, SampleType,
};
use windows::{
    Win32::{
        Devices::DeviceAndDriverInstallation::{
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
            CM_Get_Device_Interface_ListW, CR_BUFFER_SMALL, CR_SUCCESS,
        },
        Foundation::{
            CloseHandle, ERROR_BAD_COMMAND, GENERIC_READ, GENERIC_WRITE, HANDLE,
            HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING,
        },
        System::{
            IO::DeviceIoControl,
            Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery},
            Threading::{
                CREATE_NO_WINDOW, CreateEventW, CreateProcessW, GetExitCodeProcess,
                PROCESS_INFORMATION, STARTUPINFOW, WaitForSingleObject,
            },
        },
    },
    core::{GUID, HSTRING, PCWSTR, PWSTR},
};

const USAGE: &str = "usage: handle-check [--seconds N] (--close | --holder [--close-own-handle] | --detach-from-child)";

/// The check to run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// `--close`.
    Close,
    /// `--holder`.
    Holder,
    /// `--holder --close-own-handle`.
    HolderOnly,
    /// `--detach-from-child`.
    DetachFromChild,
    /// `--send-detach` (internal): the child of `--detach-from-child`.
    SendDetach(u64),
}

/// Command-line options.
struct Options {
    seconds: u64,
    mode: Mode,
}

fn parse() -> Result<Options, String> {
    let mut seconds = 30;
    let mut mode = None;
    let mut close_own_handle = false;
    let mut set = |next: Mode| {
        if mode.replace(next).is_some() {
            Err("one check at a time".to_owned())
        } else {
            Ok(())
        }
    };
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seconds" => {
                let value = args.next().ok_or("--seconds needs a value")?;
                seconds = value
                    .parse()
                    .map_err(|_| format!("--seconds: not a number: {value}"))?;
            }
            "--close" => set(Mode::Close)?,
            "--holder" => set(Mode::Holder)?,
            "--close-own-handle" => close_own_handle = true,
            "--detach-from-child" => set(Mode::DetachFromChild)?,
            "--send-detach" => {
                let value = args.next().ok_or("--send-detach needs a handle value")?;
                set(Mode::SendDetach(value.parse().map_err(|_| {
                    format!("--send-detach: not a number: {value}")
                })?))?;
            }
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }
    let mode = match (mode, close_own_handle) {
        (None, _) => return Err("no check given".to_owned()),
        (Some(Mode::Holder), true) => Mode::HolderOnly,
        (Some(_), true) => return Err("--close-own-handle needs --holder".to_owned()),
        (Some(mode), false) => mode,
    };
    Ok(Options { seconds, mode })
}

fn main() -> ExitCode {
    let options = match parse() {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if let Mode::SendDetach(handle) = options.mode {
        return send_detach(handle);
    }
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
        event: event.0.0.addr() as u64,
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
    match options.mode {
        Mode::Close => {
            drop(device);
            println!("host       closed its only handle, without DETACH");
            return match watch_mapping(&reply, options.seconds) {
                Some(false) => Ok(()),
                _ => Err("the mapping outlived the host's only handle".to_owned()),
            };
        }
        Mode::Holder | Mode::HolderOnly => {
            let holder = spawn_holder(&device)?;
            println!("holder     pid {holder} inherited the host's handle");
            if options.mode == Mode::HolderOnly {
                drop(device);
                println!("host       closed its own handle: the holder's is the last");
                // Killing the holder is the check: what the mapping does then is printed.
                watch_mapping(&reply, options.seconds);
                return Ok(());
            }
        }
        Mode::DetachFromChild => detach_from_child(&device)?,
        Mode::SendDetach(_) => unreachable!("handled in main"),
    }
    if options.mode == Mode::DetachFromChild && watch_mapping(&reply, options.seconds) != Some(true)
    {
        return Err("the mapping did not last while the host lives".to_owned());
    } else if options.mode != Mode::DetachFromChild {
        watch_mapping(&reply, options.seconds);
    }
    device
        .control(IOCTL_DETACH, &[], &mut [])
        .map_err(|e| format!("DETACH: {e}"))?;
    println!("detached");
    Ok(())
}

/// Starts a process that inherits `device`'s handle and lives for an hour; returns its id.
fn spawn_holder(device: &Handle) -> Result<u32, String> {
    let (pid, process) = spawn_inheriting(device, "ping.exe -n 3600 127.0.0.1")?;
    // The holder runs on its own.
    drop(process);
    Ok(pid)
}

/// `--detach-from-child`: a copy of this program inherits `device`'s handle and sends `DETACH`
/// with it from its own process; the driver must refuse it. Waits for the child's verdict, its
/// exit code (see [`send_detach`]).
fn detach_from_child(device: &Handle) -> Result<(), String> {
    let program = env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let command = format!(
        "\"{}\" --send-detach {}",
        program.display(),
        device.0.0.addr()
    );
    let (pid, process) = spawn_inheriting(device, &command)?;
    // SAFETY: a process handle this program owns.
    let wait = unsafe { WaitForSingleObject(process.0, 10_000) };
    if wait != WAIT_OBJECT_0 {
        return Err(format!("the child (pid {pid}) did not finish"));
    }
    let mut code = 0u32;
    // SAFETY: a process handle with query access (from `CreateProcessW`); a writable code.
    unsafe { GetExitCodeProcess(process.0, &raw mut code) }
        .map_err(|e| format!("GetExitCodeProcess: {e}"))?;
    match code {
        0 => {
            println!("child      pid {pid}: DETACH with the inherited handle refused, as it must");
            Ok(())
        }
        1 => Err(format!(
            "child pid {pid}: DETACH with the inherited handle succeeded: the driver let another process detach the host"
        )),
        _ => Err(format!("child pid {pid}: exit code {code}")),
    }
}

/// `--send-detach`: sends `DETACH` with the inherited handle `value`. Exit code 0 when the
/// driver refuses it as from another process than the host's (`ERROR_BAD_COMMAND`), 1 when it
/// accepts it, 2 on any other outcome. The child has no console: its exit code is its report.
fn send_detach(value: u64) -> ExitCode {
    let Ok(value) = usize::try_from(value) else {
        return ExitCode::from(2);
    };
    let handle = HANDLE(core::ptr::without_provenance_mut(value));
    let mut returned = 0u32;
    // SAFETY: a handle value inherited from the parent, used for one synchronous request
    // without buffers; a writable byte count. A wrong value only fails the call.
    let result = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DETACH,
            None,
            0,
            None,
            0,
            Some(&raw mut returned),
            None,
        )
    };
    match result {
        Ok(()) => ExitCode::from(1),
        Err(error) if error.code() == ERROR_BAD_COMMAND.to_hresult() => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(2),
    }
}

/// Starts `command` with `device`'s handle inheritable; returns the process's id and handle.
fn spawn_inheriting(device: &Handle, command: &str) -> Result<(u32, Handle), String> {
    // SAFETY: an open handle this process owns.
    unsafe { SetHandleInformation(device.0, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) }
        .map_err(|e| format!("SetHandleInformation: {e}"))?;
    let mut command: Vec<u16> = command.encode_utf16().chain([0]).collect();
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
    drop(Handle(process.hThread));
    Ok((process.dwProcessId, Handle(process.hProcess)))
}

/// Prints whether the shared buffer is still mapped at the address `ATTACH` returned, each time
/// that changes, for `seconds` seconds (at least once); returns the last state. Only queries
/// the address, never reads it.
fn watch_mapping(reply: &AttachReply, seconds: u64) -> Option<bool> {
    let Ok(address) = usize::try_from(reply.buffer) else {
        return None;
    };
    let end = Instant::now() + Duration::from_secs(seconds);
    let mut last = None;
    loop {
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
        if Instant::now() >= end {
            return last;
        }
        std::thread::sleep(Duration::from_millis(100));
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
        // Only a list that grew is worth another try.
        if status != CR_BUFFER_SMALL {
            return Err(format!(
                "CM_Get_Device_Interface_ListW: CONFIGRET {}",
                status.0
            ));
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
