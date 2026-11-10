//! `handle-check`: checks the hostmic driver's handle rules, with raw requests rather than the
//! `hostmic` crate (whose `Host` always detaches on drop).
//!
//! ```text
//! handle-check [--seconds N] (--close | --holder [--close-own-handle] | --detach-from-child)
//! handle-check --exit-race ROUNDS
//! handle-check --clone
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
//! And a host's exit must wait for a detach in progress elsewhere: `--exit-race ROUNDS` runs
//! that many rounds of a host (a copy of this program, `--race-host`, internal) that hands its
//! handle to a child (`--race-close`, internal), waits until the child is ready, closes its own
//! handle, lets the child go and ends at once; the child closes the last handle 0 to 1999
//! microseconds later, so that the driver undoes the mapping in the host's process while it
//! ends. The child tells whether the host's process was still ending when its close began; each
//! round, this program then attaches and detaches, which shows the host was detached. It fails
//! when no close overlapped an exit. Run it under Driver Verifier: a wrong order shows as a bug
//! check, not as an exit status.
//!
//! And the mapping must not outlive the host in a clone of its address space: `--clone` (see
//! the `clone` module).
//!
//! Exit status: 0 on success, 1 on failure, 2 on a usage error.

mod clone;

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
            CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_BAD_COMMAND, GENERIC_READ,
            GENERIC_WRITE, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0,
            WAIT_TIMEOUT,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING,
        },
        System::{
            IO::DeviceIoControl,
            Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery},
            Threading::{
                CREATE_NO_WINDOW, CreateEventW, CreateProcessW, GetCurrentProcess,
                GetExitCodeProcess, PROCESS_INFORMATION, STARTUPINFOW, SetEvent, TerminateProcess,
                WaitForMultipleObjects, WaitForSingleObject,
            },
        },
    },
    core::{GUID, HSTRING, PCWSTR, PWSTR},
};

const USAGE: &str = "usage: handle-check [--seconds N] (--close | --holder [--close-own-handle] | --detach-from-child)
       handle-check --exit-race ROUNDS
       handle-check --clone";

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
    /// `--exit-race ROUNDS`.
    ExitRace(u32),
    /// `--clone`.
    Clone,
    /// `--race-host DELAY ENDING ENDED` (internal): one round's host; its child waits `DELAY`
    /// microseconds and gives its verdict through the events `ENDING` and `ENDED`.
    RaceHost { delay: u32, ending: u64, ended: u64 },
    /// `--race-close DEVICE GATE READY HOST ENDING ENDED DELAY` (internal): the host's child,
    /// which closes the last handle `DEVICE` (see [`race_close`]).
    RaceClose { handles: RaceHandles, delay: u32 },
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
            "--clone" => set(Mode::Clone)?,
            "--exit-race" => set(Mode::ExitRace(number(&mut args, "--exit-race")?))?,
            "--race-host" => set(Mode::RaceHost {
                delay: number(&mut args, "--race-host")?,
                ending: number(&mut args, "--race-host")?,
                ended: number(&mut args, "--race-host")?,
            })?,
            "--race-close" => set(Mode::RaceClose {
                handles: RaceHandles {
                    device: number(&mut args, "--race-close")?,
                    gate: number(&mut args, "--race-close")?,
                    ready: number(&mut args, "--race-close")?,
                    host: number(&mut args, "--race-close")?,
                    ending: number(&mut args, "--race-close")?,
                    ended: number(&mut args, "--race-close")?,
                },
                delay: number(&mut args, "--race-close")?,
            })?,
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

/// The next argument, as a number, for `option`.
fn number<T: core::str::FromStr>(
    args: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<T, String> {
    let value = args.next().ok_or(format!("{option} needs a value"))?;
    value
        .parse()
        .map_err(|_| format!("{option}: not a number: {value}"))
}

fn main() -> ExitCode {
    let options = match parse() {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match options.mode {
        Mode::SendDetach(handle) => return send_detach(handle),
        Mode::RaceClose { handles, delay } => return race_close(handles, delay),
        Mode::RaceHost {
            delay,
            ending,
            ended,
        } => race_host(delay, ending, ended),
        Mode::ExitRace(rounds) => exit_race(rounds),
        Mode::Clone => clone::clone_check(),
        _ => run(&options),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(options: &Options) -> Result<(), String> {
    let path = first_interface()?;
    println!("interface  {path}");
    let device = Handle::open(&path).map_err(|e| format!("open: {e}"))?;
    let (reply, _event) = attach(&device)?;
    println!(
        "attached   buffer {:#x}, {} bytes",
        reply.buffer, reply.buffer_bytes
    );
    check(options, device, &reply)
}

/// The first present host interface's path.
fn first_interface() -> Result<String, String> {
    interface_paths()?
        .into_iter()
        .next()
        .ok_or_else(|| "no hostmic host interface: is the driver installed and started?".to_owned())
}

/// Attaches `device` as the host; returns the reply and the host's event, which must live as
/// long as the attachment.
fn attach(device: &Handle) -> Result<(AttachReply, Handle), String> {
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
    Ok((AttachReply::from_bytes(&reply), event))
}

/// The checks of `--close`, `--holder` and `--detach-from-child`, with `device` attached.
fn check(options: &Options, device: Handle, reply: &AttachReply) -> Result<(), String> {
    match options.mode {
        Mode::Close => {
            drop(device);
            println!("host       closed its only handle, without DETACH");
            return match watch_mapping(reply, options.seconds) {
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
                watch_mapping(reply, options.seconds);
                return Ok(());
            }
        }
        Mode::DetachFromChild => detach_from_child(&device)?,
        Mode::SendDetach(_)
        | Mode::ExitRace(_)
        | Mode::Clone
        | Mode::RaceHost { .. }
        | Mode::RaceClose { .. } => {
            unreachable!("handled in main")
        }
    }
    if options.mode == Mode::DetachFromChild && watch_mapping(reply, options.seconds) != Some(true)
    {
        return Err("the mapping did not last while the host lives".to_owned());
    } else if options.mode != Mode::DetachFromChild {
        watch_mapping(reply, options.seconds);
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

/// `--exit-race`: `rounds` rounds of a host that exits while its child closes the last handle
/// (see [`race_host`]); after each, attaches and detaches to show the host was detached. Fails
/// when no round's close began while the host was still ending: the race was not exercised.
fn exit_race(rounds: u32) -> Result<(), String> {
    let program = env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let path = first_interface()?;
    println!("interface  {path}");
    // The child's verdict for each round: it closed the last handle while the host was still
    // ending (`ending`), or after the host had ended (`ended`). Inherited by the host and its
    // child; auto-reset, so each wait takes one round's verdict.
    let ending = event(false)?;
    let ended = event(false)?;
    let (mut overlapped, mut after) = (0u32, 0u32);
    for round in 0..rounds {
        // The child's delay sweeps 0 to 1999 microseconds after the host's gate opens.
        let delay = round.wrapping_mul(37) % 2000;
        let command = format!(
            "\"{}\" --race-host {delay} {} {}",
            program.display(),
            ending.0.0.addr(),
            ended.0.0.addr()
        );
        let (pid, process) = spawn(&command, true)?;
        // SAFETY: a process handle this program owns.
        if unsafe { WaitForSingleObject(process.0, 10_000) } != WAIT_OBJECT_0 {
            return Err(format!(
                "round {round}: the host (pid {pid}) did not finish"
            ));
        }
        let mut code = 0u32;
        // SAFETY: a process handle with query access (from `CreateProcessW`); a writable code.
        unsafe { GetExitCodeProcess(process.0, &raw mut code) }
            .map_err(|e| format!("GetExitCodeProcess: {e}"))?;
        if code != 0 {
            return Err(format!(
                "round {round}: the host (pid {pid}) exited with {code}"
            ));
        }
        // SAFETY: two events this program owns.
        let verdict = unsafe { WaitForMultipleObjects(&[ending.0, ended.0], false, 10_000) };
        match verdict.0.wrapping_sub(WAIT_OBJECT_0.0) {
            0 => overlapped += 1,
            1 => after += 1,
            _ => {
                return Err(format!(
                    "round {round}: the host's child did not close its handle"
                ));
            }
        }
        // The child has closed the last handle: the host must be detached.
        let device = Handle::open(&path).map_err(|e| format!("open: {e}"))?;
        let (_, _event) =
            attach(&device).map_err(|e| format!("round {round}: the host stayed attached: {e}"))?;
        device
            .control(IOCTL_DETACH, &[], &mut [])
            .map_err(|e| format!("round {round}: DETACH: {e}"))?;
        if (round + 1) % 100 == 0 {
            println!(
                "exit-race  {} rounds, {overlapped} closed while the host was ending",
                round + 1
            );
        }
    }
    println!(
        "exit-race  {rounds} rounds: the host was detached each time; the last handle was closed while the host was ending in {overlapped}, after it had ended in {after}"
    );
    if overlapped == 0 {
        return Err("no close overlapped the host's exit: the race was not exercised".to_owned());
    }
    Ok(())
}

/// `--race-host`: attaches, hands the handle to a child (`--race-close`) and waits until the
/// child is ready, closes its own handle, opens the child's gate and ends at once: the child's
/// close, the last one, detaches the host in this process while it ends. `ending` and `ended`
/// are the parent's verdict events, passed on to the child.
fn race_host(delay: u32, ending: u64, ended: u64) -> Result<(), String> {
    let device = Handle::open(&first_interface()?).map_err(|e| format!("open: {e}"))?;
    let (_reply, _event) = attach(&device)?;
    let gate = event(true)?;
    let ready = event(true)?;
    // SAFETY: no precondition; returns a pseudo-handle.
    let current = unsafe { GetCurrentProcess() };
    let mut me = HANDLE::default();
    // SAFETY: duplicates this process's pseudo-handle into a real, inheritable handle, which
    // the child waits on to tell whether this process has ended.
    unsafe {
        DuplicateHandle(
            current,
            current,
            current,
            &raw mut me,
            0,
            true,
            DUPLICATE_SAME_ACCESS,
        )
    }
    .map_err(|e| format!("DuplicateHandle: {e}"))?;
    let me = Handle(me);
    let program = env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let command = format!(
        "\"{}\" --race-close {} {} {} {} {ending} {ended} {delay}",
        program.display(),
        device.0.0.addr(),
        gate.0.0.addr(),
        ready.0.0.addr(),
        me.0.0.addr(),
    );
    let (pid, child) = spawn_inheriting(&device, &command)?;
    // SAFETY: an event this process owns.
    if unsafe { WaitForSingleObject(ready.0, 10_000) } != WAIT_OBJECT_0 {
        return Err(format!("the child (pid {pid}) never got ready"));
    }
    drop(child);
    drop(device);
    // SAFETY: an event this process owns.
    unsafe { SetEvent(gate.0) }.map_err(|e| format!("SetEvent: {e}"))?;
    // Ends now, without running anything else in user mode: the race is with this exit.
    // SAFETY: terminates this process; the kernel closes its handles.
    let _ = unsafe { TerminateProcess(current, 0) };
    std::process::exit(0)
}

/// The handles `--race-close` inherits from the host, as values.
#[derive(Clone, Copy, PartialEq, Eq)]
struct RaceHandles {
    device: u64,
    gate: u64,
    ready: u64,
    host: u64,
    ending: u64,
    ended: u64,
}

/// `--race-close`: says it is ready, waits for the gate, then `delay` microseconds, then
/// closes `device`, the host's last handle, and gives the verdict: whether the host's process
/// had already ended when the close began. Exit code 0, or 2 when a handle is unusable.
fn race_close(handles: RaceHandles, delay: u32) -> ExitCode {
    let raw = |value: u64| {
        usize::try_from(value)
            .ok()
            .map(|value| HANDLE(core::ptr::without_provenance_mut(value)))
    };
    let (Some(device), Some(gate), Some(ready), Some(host), Some(ending), Some(ended)) = (
        raw(handles.device),
        raw(handles.gate),
        raw(handles.ready),
        raw(handles.host),
        raw(handles.ending),
        raw(handles.ended),
    ) else {
        return ExitCode::from(2);
    };
    // SAFETY: an inherited event handle; a wrong value only fails the call.
    if unsafe { SetEvent(ready) }.is_err() {
        return ExitCode::from(2);
    }
    // SAFETY: as above.
    if unsafe { WaitForSingleObject(gate, 10_000) } != WAIT_OBJECT_0 {
        return ExitCode::from(2);
    }
    let until = Instant::now() + Duration::from_micros(u64::from(delay));
    while Instant::now() < until {
        core::hint::spin_loop();
    }
    // SAFETY: an inherited process handle; a wrong value only fails the wait.
    let still_ending = unsafe { WaitForSingleObject(host, 0) } == WAIT_TIMEOUT;
    drop(Handle(device));
    // SAFETY: an inherited event handle; a wrong value only fails the call.
    let told = unsafe { SetEvent(if still_ending { ending } else { ended }) };
    if told.is_err() {
        return ExitCode::from(2);
    }
    ExitCode::SUCCESS
}

/// A new unnamed event, not signaled, inheritable; manual-reset if `manual`.
fn event(manual: bool) -> Result<Handle, String> {
    // SAFETY: no security attributes; an unnamed event, initially not signaled.
    let event = unsafe { CreateEventW(None, manual, false, PCWSTR::null()) }
        .map(Handle)
        .map_err(|e| format!("CreateEventW: {e}"))?;
    // SAFETY: an open handle this process owns.
    unsafe { SetHandleInformation(event.0, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) }
        .map_err(|e| format!("SetHandleInformation: {e}"))?;
    Ok(event)
}

/// Starts `command` with `device`'s handle inheritable; returns the process's id and handle.
fn spawn_inheriting(device: &Handle, command: &str) -> Result<(u32, Handle), String> {
    // SAFETY: an open handle this process owns.
    unsafe { SetHandleInformation(device.0, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) }
        .map_err(|e| format!("SetHandleInformation: {e}"))?;
    spawn(command, true)
}

/// Starts `command`, inheriting this process's inheritable handles if `inherit`; returns the
/// process's id and handle.
fn spawn(command: &str, inherit: bool) -> Result<(u32, Handle), String> {
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
            inherit,
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
