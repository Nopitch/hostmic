//! `ioctl-fuzz`: fuzzes the hostmic driver's host interface from user mode, in the test VM,
//! under Driver Verifier. cargo-fuzz covers the driver's pure validation (`fuzz/`); this covers
//! the kernel code around it, which Microsoft's Device Fundamentals fuzzers leave out (the host
//! interface is a raw device).
//!
//! ```text
//! ioctl-fuzz [--seconds N] [--seed S] [--threads T] [--processes P]
//! ```
//!
//! Phases, in order:
//!
//! - `opens`: read-only, write-only and read-write opens; the access each allows; opens in the
//!   device's namespace (a trailing name) and relative to an open handle, refused; a process at
//!   low integrity may open for reading only.
//! - `codes`: every vendor function code (0x800 to 0xFFF) with every method and access, plus
//!   random codes of other device types, with random buffers: only the six host interface codes
//!   may succeed.
//! - `sizes`: the six codes with every input and output size around theirs, invalid buffer
//!   addresses, and `ATTACH` with handle values that are not a usable event.
//! - `requests`: other requests on the handle (read, write, flush, lock, query and set
//!   information), and overlapped requests cancelled at once.
//! - `stress`, for `N` seconds (default 30): `T` threads here and `P` processes, each with its
//!   own handles, attach, advance, detach, read the state and counters, open and close handles,
//!   hand handles to short-lived processes and exit abruptly while attached; this program kills
//!   a process at random every few hundred milliseconds and starts another. Meanwhile it reads
//!   the counters, which must never go back.
//!
//! At the end, the interface must be free (an `ATTACH` succeeds, then `DETACH`). A bug check
//! or a Driver Verifier stop is the other outcome; the seed, printed first, replays the same
//! choices (the timing differs). Run it with no other host attached.
//!
//! Exit status: 0 when nothing unexpected happened, 1 otherwise, 2 on a usage error.

use std::{
    env,
    ffi::c_void,
    process::ExitCode,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use hostmic_abi::{
    ABI_VERSION, AdvanceReply, AdvanceRequest, AttachReply, AttachRequest, FormatRange,
    GUID_DEVINTERFACE_HOSTMIC_HOST, IOCTL_ADVANCE, IOCTL_ATTACH, IOCTL_DETACH, IOCTL_GET_INFO,
    IOCTL_GET_STATS, IOCTL_GET_STREAM, Info, SampleType, Stats, StreamState,
};
use windows::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
        },
    },
    Win32::{
        Devices::DeviceAndDriverInstallation::{
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
            CM_Get_Device_Interface_ListW, CR_BUFFER_SMALL, CR_SUCCESS,
        },
        Foundation::{
            CloseHandle, DUPLICATE_HANDLE_OPTIONS, DuplicateHandle, ERROR_ACCESS_DENIED,
            ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE, HANDLE_FLAG_INHERIT,
            HANDLE_FLAGS, HLOCAL, LocalFree, SetHandleInformation, UNICODE_STRING, WAIT_OBJECT_0,
        },
        Security::{
            Authorization::ConvertStringSidToSidW, DuplicateTokenEx, GetLengthSid,
            GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, PSID,
            SID_AND_ATTRIBUTES, SecurityImpersonation, SetTokenInformation, TOKEN_ACCESS_MASK,
            TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_MANDATORY_LABEL,
            TOKEN_QUERY, TokenIntegrityLevel, TokenPrimary,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_ACCESS_RIGHTS, FILE_ATTRIBUTE_NORMAL, FILE_BASIC_INFO,
            FILE_FLAG_OVERLAPPED, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
            FileBasicInfo, FlushFileBuffers, GetFileInformationByHandleEx, LockFile, OPEN_EXISTING,
            ReadFile, SetFileInformationByHandle, WriteFile,
        },
        System::{
            IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, IO_STATUS_BLOCK, OVERLAPPED},
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
                JobObjectBasicAccountingInformation, QueryInformationJobObject, TerminateJobObject,
            },
            Memory::{MEM_RESERVE, PAGE_NOACCESS, VirtualAlloc},
            SystemServices::SE_GROUP_INTEGRITY,
            Threading::{
                CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateEventW, CreateMutexW,
                CreateProcessAsUserW, CreateProcessW, GetCurrentProcess, GetExitCodeProcess,
                OpenProcessToken, PROCESS_INFORMATION, ResumeThread, STARTUPINFOW,
                TerminateProcess, WaitForSingleObject,
            },
        },
    },
    core::{GUID, HSTRING, PCWSTR, PWSTR},
};

const USAGE: &str = "usage: ioctl-fuzz [--seconds N] [--seed S] [--threads T] [--processes P]";

/// The six host interface codes, with their input and output sizes.
const CODES: [(u32, &str, usize, usize); 6] = [
    (IOCTL_GET_INFO, "GET_INFO", 0, Info::SIZE),
    (
        IOCTL_ATTACH,
        "ATTACH",
        AttachRequest::SIZE,
        AttachReply::SIZE,
    ),
    (IOCTL_GET_STREAM, "GET_STREAM", 0, StreamState::SIZE),
    (
        IOCTL_ADVANCE,
        "ADVANCE",
        AdvanceRequest::SIZE,
        AdvanceReply::SIZE,
    ),
    (IOCTL_DETACH, "DETACH", 0, 0),
    (IOCTL_GET_STATS, "GET_STATS", 0, Stats::SIZE),
];

/// The largest buffer a request gets.
const BUFFER: usize = 65536;

/// The exit code this program gives the processes it kills.
const KILLED: u32 = 0xDEAD;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let mut options = Options {
        seconds: 30,
        seed: None,
        threads: 4,
        processes: 3,
    };
    while let Some(arg) = args.next() {
        let parsed = match arg.as_str() {
            "--seconds" => number(&mut args, &arg).map(|v| options.seconds = v),
            "--seed" => number(&mut args, &arg).map(|v| options.seed = Some(v)),
            "--threads" => number(&mut args, &arg).map(|v| options.threads = v),
            "--processes" => number(&mut args, &arg).map(|v| options.processes = v),
            // Internal: a stress process, and the low-integrity probe.
            "--worker" => {
                return match (number(&mut args, &arg), number(&mut args, &arg)) {
                    (Ok(seed), Ok(seconds)) => worker(seed, seconds),
                    _ => ExitCode::from(2),
                };
            }
            "--low-probe" => return low_probe(),
            // Internal: holds the handles it inherited for MS milliseconds.
            "--hold" => {
                let milliseconds = number(&mut args, &arg).unwrap_or(0);
                thread::sleep(Duration::from_millis(milliseconds));
                return ExitCode::SUCCESS;
            }
            _ => Err(format!("unknown argument: {arg}")),
        };
        if let Err(error) = parsed {
            eprintln!("{error}\n{USAGE}");
            return ExitCode::from(2);
        }
    }
    match run(&options) {
        Ok(0) => {
            println!("PASS");
            ExitCode::SUCCESS
        }
        Ok(findings) => {
            println!("FAIL       {findings} unexpected result(s)");
            ExitCode::FAILURE
        }
        Err(error) => {
            println!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Command-line options.
struct Options {
    seconds: u64,
    seed: Option<u64>,
    threads: u32,
    processes: u32,
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

/// Runs the phases; returns the number of unexpected results.
fn run(options: &Options) -> Result<u32, String> {
    let seed = options.seed.unwrap_or_else(|| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        now.as_secs() ^ (u64::from(now.subsec_nanos()) << 20)
    });
    println!("seed       {seed}");
    let path = interface_path()?;
    println!("interface  {path}");
    let mut rng = Rng::new(seed);
    let mut findings = Findings::default();
    opens(&path, &mut findings)?;
    codes(&path, &mut rng, &mut findings)?;
    sizes(&path, &mut rng, &mut findings)?;
    requests(&path, &mut rng, &mut findings)?;
    stress(&path, options, rng.next(), &mut findings)?;
    // The interface must be free: every host above detached or died.
    let device = open(&path, GENERIC_READ.0 | GENERIC_WRITE.0, false)
        .map_err(|e| format!("open at the end: {e}"))?;
    let end = Instant::now() + Duration::from_secs(10);
    let event = loop {
        match attach(&device) {
            Ok((_, event)) => break event,
            Err(error) if Instant::now() >= end => {
                findings.note(&format!("end: the interface stayed busy: {error}"));
                break None;
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    };
    if event.is_some() && control(&device, IOCTL_DETACH, &[], &mut []).is_err() {
        findings.note("end: DETACH after the final ATTACH failed");
    }
    println!("end        the interface is free");
    Ok(findings.count)
}

/// Unexpected results, printed as they come (the first 50).
#[derive(Default)]
struct Findings {
    count: u32,
}

impl Findings {
    fn note(&mut self, what: &str) {
        self.count += 1;
        if self.count <= 50 {
            println!("UNEXPECTED {what}");
        }
    }
}

/// `opens`: what each kind of open allows.
fn opens(path: &str, findings: &mut Findings) -> Result<(), String> {
    let both = open(path, GENERIC_READ.0 | GENERIC_WRITE.0, false)
        .map_err(|e| format!("read-write open: {e}"))?;
    let read = open(path, GENERIC_READ.0, false).map_err(|e| format!("read open: {e}"))?;
    let write = open(path, GENERIC_WRITE.0, false).map_err(|e| format!("write open: {e}"))?;
    let mut info = [0u8; Info::SIZE];
    if control(&read, IOCTL_GET_INFO, &[], &mut info).is_err() {
        findings.note("opens: GET_INFO refused on a read handle");
    }
    if control(&write, IOCTL_GET_INFO, &[], &mut info) != Err(ERROR_ACCESS_DENIED.to_hresult().0) {
        findings.note("opens: GET_INFO not refused as access denied on a write-only handle");
    }
    let request = attach_request(0).to_bytes();
    let mut reply = [0u8; AttachReply::SIZE];
    if control(&read, IOCTL_ATTACH, &request, &mut reply) != Err(ERROR_ACCESS_DENIED.to_hresult().0)
    {
        findings.note("opens: ATTACH not refused as access denied on a read-only handle");
    }
    // A name after the device's: FILE_DEVICE_SECURE_OPEN and the driver refuse it.
    if open(&format!("{path}\\x"), GENERIC_READ.0, false).is_ok() {
        findings.note("opens: an open with a name in the device's namespace succeeded");
    }
    // Relative to an open handle, with and without a name: the driver refuses both.
    for name in ["", "x"] {
        if relative_open(&both, name).is_ok() {
            findings.note(&format!(
                "opens: an open relative to a handle ({name:?}) succeeded"
            ));
        }
    }
    drop((both, read, write));
    match low_integrity(path) {
        Ok(0) => println!("opens      low integrity: reading allowed, writing refused"),
        Ok(code) => findings.note(&format!(
            "opens: the low-integrity probe reported {code:#x}"
        )),
        Err(error) => println!("opens      low integrity: not run ({error})"),
    }
    println!("opens      done");
    Ok(())
}

/// Opens `path` relative to `root`'s file object, with `name`.
fn relative_open(root: &Handle, name: &str) -> Result<Handle, i32> {
    let mut wide: Vec<u16> = name.encode_utf16().collect();
    let bytes = u16::try_from(wide.len() * 2).unwrap_or(0);
    let name = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: PWSTR(wide.as_mut_ptr()),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>()).unwrap_or(0),
        RootDirectory: root.0,
        ObjectName: &raw const name,
        ..Default::default()
    };
    let mut handle = HANDLE::default();
    let mut status = IO_STATUS_BLOCK::default();
    // SAFETY: valid out-pointers, object attributes and name that live through the call.
    let result = unsafe {
        NtCreateFile(
            &raw mut handle,
            FILE_ACCESS_RIGHTS(GENERIC_READ.0),
            &raw const attributes,
            &raw mut status,
            None,
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT,
            None,
            0,
        )
    };
    if result.is_ok() {
        Ok(Handle(handle))
    } else {
        Err(result.0)
    }
}

/// Starts this program at low integrity (`--low-probe`) and returns its exit code: 0 when it
/// could open for reading and read, and could not open for writing.
fn low_integrity(path: &str) -> Result<u32, String> {
    let mut token = HANDLE::default();
    // SAFETY: the current process's pseudo-handle; a writable handle.
    let current = unsafe { GetCurrentProcess() };
    let access = TOKEN_DUPLICATE | TOKEN_ADJUST_DEFAULT | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY;
    // SAFETY: as above.
    unsafe { OpenProcessToken(current, access, &raw mut token) }
        .map_err(|e| format!("OpenProcessToken: {e}"))?;
    let token = Handle(token);
    let mut low = HANDLE::default();
    // SAFETY: an open token; a writable handle.
    unsafe {
        DuplicateTokenEx(
            token.0,
            TOKEN_ACCESS_MASK(0),
            None,
            SecurityImpersonation,
            TokenPrimary,
            &raw mut low,
        )
    }
    .map_err(|e| format!("DuplicateTokenEx: {e}"))?;
    let low = Handle(low);
    let mut sid = PSID::default();
    // SAFETY: a NUL-terminated SID string; a writable SID pointer, freed below.
    unsafe { ConvertStringSidToSidW(&HSTRING::from("S-1-16-4096"), &raw mut sid) }
        .map_err(|e| format!("ConvertStringSidToSidW: {e}"))?;
    let label = TOKEN_MANDATORY_LABEL {
        Label: SID_AND_ATTRIBUTES {
            Sid: sid,
            Attributes: u32::try_from(SE_GROUP_INTEGRITY).unwrap_or(0),
        },
    };
    // SAFETY: a valid SID from `ConvertStringSidToSidW`.
    let sid_length = unsafe { GetLengthSid(sid) };
    let length = u32::try_from(size_of::<TOKEN_MANDATORY_LABEL>()).unwrap_or(0) + sid_length;
    // SAFETY: a primary token opened for adjusting; the label and its SID live through the call.
    let set = unsafe {
        SetTokenInformation(
            low.0,
            TokenIntegrityLevel,
            (&raw const label).cast(),
            length,
        )
    };
    // SAFETY: the SID `ConvertStringSidToSidW` allocated, no longer used.
    unsafe { LocalFree(Some(HLOCAL(sid.0))) };
    set.map_err(|e| format!("SetTokenInformation: {e}"))?;
    let program = env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut command: Vec<u16> = format!("\"{}\" --low-probe \"{path}\"", program.display())
        .encode_utf16()
        .chain([0])
        .collect();
    let startup = STARTUPINFOW {
        cb: u32::try_from(size_of::<STARTUPINFOW>()).unwrap_or(0),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: a primary token; a writable NUL-terminated command line; valid startup and
    // process information; no handles inherited.
    unsafe {
        CreateProcessAsUserW(
            Some(low.0),
            PCWSTR::null(),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &raw const startup,
            &raw mut process,
        )
    }
    .map_err(|e| format!("CreateProcessAsUserW: {e}"))?;
    drop(Handle(process.hThread));
    let process = Handle(process.hProcess);
    wait_exit(&process, 10_000).ok_or("the probe did not finish".to_owned())
}

/// `--low-probe PATH` (at low integrity): exit code bit 0 if opening for reading failed, bit 1
/// if `GET_INFO` failed on it, bit 2 if opening for reading and writing succeeded, bit 3 if the
/// probe does not run at low integrity (its token's integrity level is not 0x1000).
fn low_probe() -> ExitCode {
    let Some(path) = env::args().nth(2) else {
        return ExitCode::from(0x80);
    };
    let mut code = 0u8;
    if integrity_level() != Some(0x1000) {
        code |= 8;
    }
    match open(&path, GENERIC_READ.0, false) {
        Ok(read) => {
            let mut info = [0u8; Info::SIZE];
            if control(&read, IOCTL_GET_INFO, &[], &mut info).is_err() {
                code |= 2;
            }
        }
        Err(_) => code |= 1,
    }
    if open(&path, GENERIC_READ.0 | GENERIC_WRITE.0, false).is_ok() {
        code |= 4;
    }
    ExitCode::from(code)
}

/// This process's integrity level (the last subauthority of its token's label), or `None`.
fn integrity_level() -> Option<u32> {
    // SAFETY: the current process's pseudo-handle.
    let current = unsafe { GetCurrentProcess() };
    let mut token = HANDLE::default();
    // SAFETY: as above; a writable handle.
    unsafe { OpenProcessToken(current, TOKEN_QUERY, &raw mut token) }.ok()?;
    let token = Handle(token);
    // A TOKEN_MANDATORY_LABEL followed by its SID: 64 bytes are plenty, 8-aligned.
    let mut buffer = [0u64; 8];
    let mut length = 0u32;
    // SAFETY: an open token; a writable buffer of the given size.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenIntegrityLevel,
            Some(buffer.as_mut_ptr().cast()),
            u32::try_from(size_of_val(&buffer)).unwrap_or(0),
            &raw mut length,
        )
    }
    .ok()?;
    // SAFETY: the buffer now starts with a TOKEN_MANDATORY_LABEL (aligned: u64 storage).
    let sid = unsafe { buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>().read() }
        .Label
        .Sid;
    // SAFETY: a valid SID in `buffer`, which lives on.
    let count = unsafe { GetSidSubAuthorityCount(sid) };
    // SAFETY: points into that SID.
    let count = unsafe { count.read() };
    let last = u32::from(count.checked_sub(1)?);
    // SAFETY: a subauthority index below the SID's count.
    let level = unsafe { GetSidSubAuthority(sid, last) };
    // SAFETY: points into that SID.
    Some(unsafe { level.read() })
}

/// `codes`: every control code but the six, with random buffers, must fail.
fn codes(path: &str, rng: &mut Rng, findings: &mut Findings) -> Result<(), String> {
    let device =
        open(path, GENERIC_READ.0 | GENERIC_WRITE.0, false).map_err(|e| format!("open: {e}"))?;
    let mut input = vec![0u8; BUFFER];
    let mut output = vec![0u8; BUFFER];
    let mut sent = 0u32;
    let mut try_code = |code: u32, rng: &mut Rng, findings: &mut Findings| {
        if CODES.iter().any(|&(known, ..)| known == code) {
            return;
        }
        let in_len = rng.size();
        let out_len = rng.size();
        rng.fill(&mut input[..in_len]);
        sent += 1;
        if control(&device, code, &input[..in_len], &mut output[..out_len]).is_ok() {
            findings.note(&format!(
                "codes: {code:#010x} succeeded ({in_len} in, {out_len} out)"
            ));
        }
    };
    for function in 0x800..=0xFFF_u32 {
        for method in 0..4 {
            for access in 0..4 {
                try_code(ctl_code(0x22, function, method, access), rng, findings);
            }
        }
    }
    for _ in 0..4096 {
        let code = u32::try_from(rng.next() >> 32).unwrap_or(0);
        try_code(code, rng, findings);
    }
    println!("codes      {sent} requests");
    Ok(())
}

/// `sizes`: the six codes with every size around theirs, invalid addresses, and bad events.
fn sizes(path: &str, rng: &mut Rng, findings: &mut Findings) -> Result<(), String> {
    let device =
        open(path, GENERIC_READ.0 | GENERIC_WRITE.0, false).map_err(|e| format!("open: {e}"))?;
    let mut input = vec![0u8; BUFFER];
    let mut output = vec![0u8; BUFFER];
    let event = new_event()?;
    let mut sent = 0u32;
    for &(code, name, in_size, out_size) in &CODES {
        for in_len in around(in_size) {
            for out_len in around(out_size) {
                if code == IOCTL_ATTACH && in_len == in_size {
                    input[..in_len].copy_from_slice(&attach_request(event.value()).to_bytes());
                } else {
                    rng.fill(&mut input[..in_len]);
                }
                sent += 1;
                let result = control(&device, code, &input[..in_len], &mut output[..out_len]);
                // The input must be exactly the request's size; the output may be larger than
                // the reply (the driver returns the reply's size).
                if result.is_ok() && (in_len != in_size || out_len < out_size) {
                    findings.note(&format!(
                        "sizes: {name} succeeded with {in_len} in, {out_len} out"
                    ));
                }
                if code == IOCTL_ATTACH && result.is_ok() {
                    let _ = control(&device, IOCTL_DETACH, &[], &mut []);
                }
            }
        }
    }
    // Buffers at addresses that are reserved but not committed, in the kernel's half, or null.
    // SAFETY: reserves address space only; never touched by this program.
    let reserved = unsafe { VirtualAlloc(None, BUFFER, MEM_RESERVE, PAGE_NOACCESS) };
    let bad: [*mut c_void; 3] = [
        reserved,
        core::ptr::without_provenance_mut(0xFFFF_8000_0000_1000),
        core::ptr::null_mut(),
    ];
    // Valid requests, so that only the buffer's address is wrong.
    input[..AttachRequest::SIZE].copy_from_slice(&attach_request(event.value()).to_bytes());
    for &(code, name, in_size, out_size) in &CODES {
        for &address in &bad {
            for (in_ptr, in_len, out_ptr, out_len) in [
                (
                    address.cast_const(),
                    in_size.max(8),
                    output.as_mut_ptr().cast(),
                    out_size,
                ),
                (input.as_ptr().cast(), in_size, address, out_size.max(8)),
            ] {
                sent += 1;
                // SAFETY: the addresses are invalid on purpose; the I/O manager probes them and
                // fails the request, or the call fails in user mode: this program's memory is
                // only ever written within `output`.
                let result =
                    unsafe { raw_control(&device, code, in_ptr, in_len, out_ptr, out_len) };
                // A null buffer is sent as no buffer (Windows sets its length to 0): a request
                // without input may succeed with it. Any other address must fail.
                if result.is_ok() && !address.is_null() {
                    findings.note(&format!(
                        "sizes: {name} succeeded with an invalid buffer at {address:p}"
                    ));
                }
                if result.is_ok() && code == IOCTL_ATTACH {
                    let _ = control(&device, IOCTL_DETACH, &[], &mut []);
                }
            }
        }
    }
    // ATTACH with handle values that are not a usable event: closed, a mutex, an event without
    // EVENT_MODIFY_STATE, the device itself, pseudo-handles, values with high bits.
    let closed = new_event()?.value();
    // SAFETY: an unnamed mutex, no security attributes.
    let mutex = Handle(
        unsafe { CreateMutexW(None, false, PCWSTR::null()) }
            .map_err(|e| format!("CreateMutexW: {e}"))?,
    );
    let synchronize_only = duplicate(&event, 0x0010_0000)?;
    let values = [
        0,
        closed,
        mutex.value(),
        synchronize_only.value(),
        device.value(),
        0xFFFF_FFFF,
        0xFFFF_FFFE,
        0x8000_0004,
        0x1_0000_0004,
        u64::MAX,
    ];
    for value in values {
        sent += 1;
        let mut reply = [0u8; AttachReply::SIZE];
        if control(
            &device,
            IOCTL_ATTACH,
            &attach_request(value).to_bytes(),
            &mut reply,
        )
        .is_ok()
        {
            findings.note(&format!(
                "sizes: ATTACH succeeded with the event handle value {value:#x}"
            ));
            let _ = control(&device, IOCTL_DETACH, &[], &mut []);
        }
    }
    println!("sizes      {sent} requests");
    Ok(())
}

/// The sizes to try around `size`.
fn around(size: usize) -> Vec<usize> {
    let mut sizes = vec![0, 1, size.saturating_sub(1), size, size + 1, 4096, BUFFER];
    sizes.sort_unstable();
    sizes.dedup();
    sizes
}

/// `requests`: other requests on the handle, and overlapped requests cancelled at once.
fn requests(path: &str, rng: &mut Rng, findings: &mut Findings) -> Result<(), String> {
    let device =
        open(path, GENERIC_READ.0 | GENERIC_WRITE.0, false).map_err(|e| format!("open: {e}"))?;
    let mut buffer = [0u8; 64];
    // SAFETY: an open handle; a writable buffer; synchronous.
    if unsafe { ReadFile(device.0, Some(&mut buffer), None, None) }.is_ok() {
        findings.note("requests: ReadFile succeeded");
    }
    // SAFETY: as above.
    if unsafe { WriteFile(device.0, Some(&buffer), None, None) }.is_ok() {
        findings.note("requests: WriteFile succeeded");
    }
    // These may be answered by the I/O manager itself; they must only not crash.
    // SAFETY: an open handle.
    let _ = unsafe { FlushFileBuffers(device.0) };
    // SAFETY: an open handle.
    let _ = unsafe { LockFile(device.0, 0, 0, 1, 0) };
    let mut basic = FILE_BASIC_INFO::default();
    let basic_size = u32::try_from(size_of::<FILE_BASIC_INFO>()).unwrap_or(0);
    // SAFETY: an open handle; a writable structure of the given size.
    let _ = unsafe {
        GetFileInformationByHandleEx(device.0, FileBasicInfo, (&raw mut basic).cast(), basic_size)
    };
    // SAFETY: as above, read only.
    let _ = unsafe {
        SetFileInformationByHandle(
            device.0,
            FileBasicInfo,
            (&raw const basic).cast(),
            basic_size,
        )
    };

    // Overlapped requests, cancelled as soon as they are sent.
    let overlapped_device = open(path, GENERIC_READ.0 | GENERIC_WRITE.0, true)
        .map_err(|e| format!("overlapped open: {e}"))?;
    let event = new_event()?;
    let attach_event = new_event()?;
    let mut sent = 0u32;
    for _ in 0..2000 {
        let (code, _, in_size, out_size) = CODES[rng.index(CODES.len())];
        let mut input = [0u8; AttachRequest::SIZE];
        if code == IOCTL_ATTACH {
            input = attach_request(attach_event.value()).to_bytes();
        } else {
            rng.fill(&mut input);
        }
        let mut output = [0u8; 64];
        let mut overlapped = OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        };
        sent += 1;
        // SAFETY: an overlapped handle; buffers and the OVERLAPPED live until the request is
        // over (GetOverlappedResult waits below).
        let started = unsafe {
            DeviceIoControl(
                overlapped_device.0,
                code,
                Some(input.as_ptr().cast()),
                u32::try_from(in_size).unwrap_or(0),
                Some(output.as_mut_ptr().cast()),
                u32::try_from(out_size).unwrap_or(0),
                None,
                Some(&raw mut overlapped),
            )
        };
        let pending = matches!(&started, Err(e) if e.code() == ERROR_IO_PENDING.to_hresult());
        if started.is_ok() || pending {
            // SAFETY: the handle and the OVERLAPPED of the request just sent.
            let _ = unsafe { CancelIoEx(overlapped_device.0, Some(&raw const overlapped)) };
            let mut transferred = 0u32;
            // SAFETY: waits for that request to be over before its buffers go.
            let _ = unsafe {
                GetOverlappedResult(
                    overlapped_device.0,
                    &raw const overlapped,
                    &raw mut transferred,
                    true,
                )
            };
        }
        if code == IOCTL_ATTACH {
            // This driver completes requests at once, so a request on the overlapped handle
            // without an OVERLAPPED waits as on a synchronous one.
            let _ = control(&overlapped_device, IOCTL_DETACH, &[], &mut []);
        }
    }
    println!("requests   other requests and {sent} overlapped ones");
    Ok(())
}

/// `stress`: threads here and processes that attach, advance, detach and die.
fn stress(path: &str, options: &Options, seed: u64, findings: &mut Findings) -> Result<(), String> {
    // SAFETY: an unnamed job, no security attributes.
    let job = Handle(
        unsafe { CreateJobObjectW(None, PCWSTR::null()) }
            .map_err(|e| format!("CreateJobObjectW: {e}"))?,
    );
    let _ = JOB.set(job.0.0.addr());
    let end = Instant::now() + Duration::from_secs(options.seconds);
    let operations = Arc::new(AtomicU64::new(0));
    let threads: Vec<_> = (0..options.threads)
        .map(|index| {
            let path = path.to_owned();
            let operations = Arc::clone(&operations);
            let mut rng =
                Rng::new(seed ^ (u64::from(index) + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            thread::spawn(move || {
                let mut state = Worker::default();
                while Instant::now() < end {
                    state.step(&path, &mut rng, false);
                    operations.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    let mut rng = Rng::new(seed.rotate_left(17));
    let mut processes: Vec<Handle> = Vec::new();
    let mut spawned = 0u32;
    let mut killed = 0u32;
    let mut failed = 0u32;
    let stats_device = open(path, GENERIC_READ.0, false).map_err(|e| format!("open: {e}"))?;
    let mut last = read_stats(&stats_device);
    while Instant::now() < end {
        // Reap the processes that ended; keep `processes` of them running.
        processes.retain(|process| match wait_exit(process, 0) {
            Some(code) => {
                if code != 0 && code != KILLED {
                    failed += 1;
                }
                false
            }
            None => true,
        });
        while processes.len() < options.processes as usize {
            let seconds = 1 + rng.below(5);
            let command = format!("\"{}\" --worker {} {seconds}", program(), rng.next());
            processes.push(spawn(&command)?);
            spawned += 1;
        }
        if !processes.is_empty() && rng.below(3) == 0 {
            let victim = processes.swap_remove(rng.index(processes.len()));
            // SAFETY: a process handle this program owns, from CreateProcessW.
            let _ = unsafe { TerminateProcess(victim.0, KILLED) };
            killed += 1;
        }
        let now = read_stats(&stats_device);
        if let (Some(before), Some(after)) = (last, now)
            && (after.requests_refused < before.requests_refused
                || after.frames_advanced < before.frames_advanced
                || after.packets_completed < before.packets_completed)
        {
            findings.note("stress: a counter went back");
        }
        last = now.or(last);
        thread::sleep(Duration::from_millis(50 + rng.below(250)));
    }
    for thread in threads {
        let _ = thread.join();
    }
    // Every process started (workers, holders, their children) must end: within a few seconds
    // on its own, or once killed. One that cannot end even then is stuck in the kernel.
    let waited = Instant::now();
    let mut left = wait_job_empty(&job, 10);
    if left > 0 {
        println!("stress     {left} process(es) still running after 10 s: killing them");
        // SAFETY: the job this program created.
        let _ = unsafe { TerminateJobObject(job.0, KILLED) };
        left = wait_job_empty(&job, 20);
        if left > 0 {
            findings.note(&format!(
                "stress: {left} process(es) could not end, even killed"
            ));
        }
    }
    println!(
        "stress     every process ended {} ms after the last operation",
        waited.elapsed().as_millis()
    );
    for process in &processes {
        if let Some(code) = wait_exit(process, 0)
            && code != 0
            && code != KILLED
        {
            failed += 1;
        }
    }
    if failed > 0 {
        findings.note(&format!(
            "stress: {failed} process(es) failed (not killed by this program)"
        ));
    }
    println!(
        "stress     {} operations in {} threads, {spawned} processes ({killed} killed)",
        operations.load(Ordering::Relaxed),
        options.threads
    );
    Ok(())
}

/// `--worker SEED SECONDS`: a stress process; ends abruptly at a random point, sometimes
/// attached.
fn worker(seed: u64, seconds: u64) -> ExitCode {
    let Ok(path) = interface_path() else {
        return ExitCode::SUCCESS;
    };
    let mut rng = Rng::new(seed);
    let end = Instant::now() + Duration::from_secs(seconds);
    let mut state = Worker::default();
    while Instant::now() < end {
        state.step(&path, &mut rng, true);
    }
    // Ends with whatever handles and attachment it has: the driver cleans up.
    std::process::exit(0)
}

/// One stress actor's handles: open handles, the event it attached with.
#[derive(Default)]
struct Worker {
    handles: Vec<Handle>,
    events: Vec<Handle>,
}

impl Worker {
    /// One random operation.
    fn step(&mut self, path: &str, rng: &mut Rng, may_exit: bool) {
        match rng.below(100) {
            0..10 => {
                if self.handles.len() < 4
                    && let Ok(handle) = open(path, GENERIC_READ.0 | GENERIC_WRITE.0, false)
                {
                    self.handles.push(handle);
                }
            }
            10..15 => {
                if !self.handles.is_empty() {
                    drop(self.handles.swap_remove(rng.index(self.handles.len())));
                }
            }
            15..30 => self.attach(rng),
            30..40 => {
                if let Some(handle) = self.pick(rng) {
                    let _ = control(handle, IOCTL_DETACH, &[], &mut []);
                }
            }
            40..70 => {
                if let Some(handle) = self.pick(rng) {
                    advance(handle, rng);
                }
            }
            70..85 => {
                if let Some(handle) = self.pick(rng) {
                    let mut out = [0u8; Stats::SIZE];
                    let _ = control(handle, IOCTL_GET_STATS, &[], &mut out);
                    let mut info = [0u8; Info::SIZE];
                    let _ = control(handle, IOCTL_GET_INFO, &[], &mut info);
                }
            }
            85..92 => {
                if let Some(handle) = self.pick(rng) {
                    hand_over(handle, rng);
                }
            }
            92..99 => thread::sleep(Duration::from_millis(rng.below(5))),
            _ => {
                if may_exit && rng.below(4) == 0 {
                    // Dies with its handles open and perhaps attached.
                    std::process::exit(0);
                }
            }
        }
    }

    /// `ATTACH` on one of the handles, with a new event kept while it may be in use.
    fn attach(&mut self, rng: &mut Rng) {
        let Some(handle) = self.pick(rng) else {
            return;
        };
        let Ok(event) = new_event() else {
            return;
        };
        let mut reply = [0u8; AttachReply::SIZE];
        let request = attach_request(event.value()).to_bytes();
        if control(handle, IOCTL_ATTACH, &request, &mut reply).is_ok() {
            self.events.push(event);
            if self.events.len() > 4 {
                self.events.remove(0);
            }
        }
    }

    fn pick(&self, rng: &mut Rng) -> Option<&Handle> {
        if self.handles.is_empty() {
            None
        } else {
            self.handles.get(rng.index(self.handles.len()))
        }
    }
}

/// `GET_STREAM`, then `ADVANCE` with a valid, stale or random generation, position and size.
fn advance(handle: &Handle, rng: &mut Rng) {
    let mut stream = [0u8; StreamState::SIZE];
    let state = control(handle, IOCTL_GET_STREAM, &[], &mut stream)
        .ok()
        .map(|_| StreamState::from_bytes(&stream))
        .unwrap_or_default();
    let request = AdvanceRequest {
        generation: if rng.below(2) == 0 {
            state.generation
        } else {
            rng.next_u32()
        },
        frames: match rng.below(3) {
            0 => 48,
            1 => state.buffer_frames,
            _ => rng.next_u32(),
        },
        from: if rng.below(2) == 0 {
            state.position
        } else {
            rng.next()
        },
    };
    let mut reply = [0u8; AdvanceReply::SIZE];
    let _ = control(handle, IOCTL_ADVANCE, &request.to_bytes(), &mut reply);
}

/// A short-lived process (`--hold`, up to half a second) inherits `handle`; killed half the
/// time.
fn hand_over(handle: &Handle, rng: &mut Rng) {
    // SAFETY: an open handle this program owns.
    let inheritable =
        unsafe { SetHandleInformation(handle.0, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) };
    if inheritable.is_err() {
        return;
    }
    let command = format!("\"{}\" --hold {}", program(), rng.below(500));
    if let Ok(holder) = spawn_inheriting(&command)
        && rng.below(2) == 0
    {
        // SAFETY: a process handle this program owns.
        let _ = unsafe { TerminateProcess(holder.0, KILLED) };
    }
    // SAFETY: as above.
    let _ = unsafe { SetHandleInformation(handle.0, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) };
}

/// `GET_STATS`, or `None` when it fails.
fn read_stats(device: &Handle) -> Option<Stats> {
    let mut out = [0u8; Stats::SIZE];
    control(device, IOCTL_GET_STATS, &[], &mut out).ok()?;
    Some(Stats::from_bytes(&out))
}

/// Attaches `device` with a new event; returns the reply and the event.
fn attach(device: &Handle) -> Result<(AttachReply, Option<Handle>), String> {
    let event = new_event()?;
    let mut reply = [0u8; AttachReply::SIZE];
    control(
        device,
        IOCTL_ATTACH,
        &attach_request(event.value()).to_bytes(),
        &mut reply,
    )
    .map_err(|code| format!("ATTACH: {code:#010x}"))?;
    Ok((AttachReply::from_bytes(&reply), Some(event)))
}

/// An `ATTACH` request with the event handle value `event`, 48 kHz 16-bit stereo.
fn attach_request(event: u64) -> AttachRequest {
    AttachRequest {
        abi_version: ABI_VERSION,
        reserved: 0,
        event,
        formats: FormatRange {
            min_rate: 48_000,
            max_rate: 48_000,
            min_channels: 2,
            max_channels: 2,
            sample_types: SampleType::Int16.bit(),
        },
    }
}

/// `CTL_CODE` of `winioctl.h`.
const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

/// Something that holds a raw handle for [`control`].
trait AsRawHandle {
    fn raw(&self) -> HANDLE;
}

impl AsRawHandle for Handle {
    fn raw(&self) -> HANDLE {
        self.0
    }
}

/// A synchronous device control request; the bytes returned, or the error's `HRESULT`.
fn control(
    device: &impl AsRawHandle,
    code: u32,
    input: &[u8],
    output: &mut [u8],
) -> Result<u32, i32> {
    let in_ptr: *const c_void = if input.is_empty() {
        core::ptr::null()
    } else {
        input.as_ptr().cast()
    };
    let out_ptr: *mut c_void = if output.is_empty() {
        core::ptr::null_mut()
    } else {
        output.as_mut_ptr().cast()
    };
    // SAFETY: buffers of the given lengths that live through the synchronous call.
    unsafe { raw_control(device, code, in_ptr, input.len(), out_ptr, output.len()) }
}

/// `DeviceIoControl` with raw buffers.
///
/// # Safety
///
/// The kernel may write up to `out_len` bytes at `out_ptr` in this process: it must be a
/// writable buffer of that size, or an address the I/O manager refuses (not committed, or in
/// the kernel's half).
unsafe fn raw_control(
    device: &impl AsRawHandle,
    code: u32,
    in_ptr: *const c_void,
    in_len: usize,
    out_ptr: *mut c_void,
    out_len: usize,
) -> Result<u32, i32> {
    let mut returned = 0u32;
    // SAFETY: the caller's contract; a writable byte count.
    unsafe {
        DeviceIoControl(
            device.raw(),
            code,
            (!in_ptr.is_null()).then_some(in_ptr),
            u32::try_from(in_len).unwrap_or(u32::MAX),
            (!out_ptr.is_null()).then_some(out_ptr),
            u32::try_from(out_len).unwrap_or(u32::MAX),
            Some(&raw mut returned),
            None,
        )
    }
    .map(|()| returned)
    .map_err(|e| e.code().0)
}

/// Opens `path` with `access`, overlapped if `overlapped`.
fn open(path: &str, access: u32, overlapped: bool) -> windows::core::Result<Handle> {
    let path = HSTRING::from(path);
    let flags = if overlapped {
        FILE_FLAG_OVERLAPPED
    } else {
        FILE_FLAGS_AND_ATTRIBUTES(0)
    };
    // SAFETY: a NUL-terminated path; no security attributes or template.
    let handle = unsafe {
        CreateFileW(
            &path,
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            flags,
            None,
        )
    }?;
    Ok(Handle(handle))
}

/// A new unnamed auto-reset event.
fn new_event() -> Result<Handle, String> {
    // SAFETY: no security attributes; an unnamed auto-reset event, initially not signaled.
    unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
        .map(Handle)
        .map_err(|e| format!("CreateEventW: {e}"))
}

/// A duplicate of `handle` with only `access`.
fn duplicate(handle: &Handle, access: u32) -> Result<Handle, String> {
    // SAFETY: the current process's pseudo-handle.
    let current = unsafe { GetCurrentProcess() };
    let mut copy = HANDLE::default();
    // SAFETY: an open handle of this process; a writable handle.
    unsafe {
        DuplicateHandle(
            current,
            handle.0,
            current,
            &raw mut copy,
            access,
            false,
            DUPLICATE_HANDLE_OPTIONS(0),
        )
    }
    .map_err(|e| format!("DuplicateHandle: {e}"))?;
    Ok(Handle(copy))
}

/// Starts `command` without inheriting handles.
fn spawn(command: &str) -> Result<Handle, String> {
    start(command, false)
}

/// Starts `command`, inheriting this process's inheritable handles.
fn spawn_inheriting(command: &str) -> Result<Handle, String> {
    start(command, true)
}

/// The job object every process this program starts joins (their own children join it by
/// inheritance); set by `stress` in the first process only.
static JOB: OnceLock<usize> = OnceLock::new();

/// Starts `command`, suspended until it has joined [`JOB`] when there is one. Only the first
/// process has [`JOB`]: a worker's children join its job by inheritance, and must not start
/// suspended, since the worker may be killed before it resumes them.
fn start(command: &str, inherit: bool) -> Result<Handle, String> {
    let job = JOB
        .get()
        .map(|&job| HANDLE(core::ptr::without_provenance_mut(job)));
    let flags = if job.is_some() {
        CREATE_NO_WINDOW | CREATE_SUSPENDED
    } else {
        CREATE_NO_WINDOW
    };
    let mut command: Vec<u16> = command.encode_utf16().chain([0]).collect();
    let startup = STARTUPINFOW {
        cb: u32::try_from(size_of::<STARTUPINFOW>()).unwrap_or(0),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: a writable NUL-terminated command line; valid startup and process information.
    unsafe {
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            inherit,
            flags,
            None,
            PCWSTR::null(),
            &raw const startup,
            &raw mut process,
        )
    }
    .map_err(|e| format!("CreateProcessW: {e}"))?;
    let thread = Handle(process.hThread);
    let process = Handle(process.hProcess);
    if let Some(job) = job {
        // SAFETY: the job this program created and a process it just started.
        if let Err(error) = unsafe { AssignProcessToJobObject(job, process.0) } {
            // SAFETY: a process handle this program owns.
            let _ = unsafe { TerminateProcess(process.0, KILLED) };
            return Err(format!("AssignProcessToJobObject: {error}"));
        }
        // SAFETY: the main thread of the process just started, suspended.
        unsafe { ResumeThread(thread.0) };
    }
    Ok(process)
}

/// Processes still running in [`JOB`].
fn job_processes(job: &Handle) -> Option<u32> {
    let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    // SAFETY: a job this program owns; a writable structure of the given size.
    unsafe {
        QueryInformationJobObject(
            Some(job.0),
            JobObjectBasicAccountingInformation,
            (&raw mut info).cast(),
            u32::try_from(size_of_val(&info)).unwrap_or(0),
            None,
        )
    }
    .ok()?;
    Some(info.ActiveProcesses)
}

/// Waits up to `seconds` for [`JOB`] to have no process left; how many are left.
fn wait_job_empty(job: &Handle, seconds: u64) -> u32 {
    let end = Instant::now() + Duration::from_secs(seconds);
    loop {
        let left = job_processes(job).unwrap_or(u32::MAX);
        if left == 0 || Instant::now() >= end {
            return left;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// This program's path, for the processes it starts.
fn program() -> &'static str {
    static PROGRAM: OnceLock<String> = OnceLock::new();
    PROGRAM.get_or_init(|| {
        env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_default()
    })
}

/// Waits up to `milliseconds` for `process` to end; its exit code, or `None` if it still runs.
fn wait_exit(process: &Handle, milliseconds: u32) -> Option<u32> {
    // SAFETY: a process handle this program owns.
    if unsafe { WaitForSingleObject(process.0, milliseconds) } != WAIT_OBJECT_0 {
        return None;
    }
    let mut code = 0u32;
    // SAFETY: as above; a writable code.
    unsafe { GetExitCodeProcess(process.0, &raw mut code) }.ok()?;
    Some(code)
}

/// The first present host interface's path.
fn interface_path() -> Result<String, String> {
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
            return buffer
                .split(|&unit| unit == 0)
                .find(|path| !path.is_empty())
                .map(String::from_utf16_lossy)
                .ok_or_else(|| {
                    "no hostmic host interface: is the driver installed and started?".to_owned()
                });
        }
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
    /// The handle's value, as the ABI carries it.
    fn value(&self) -> u64 {
        self.0.0.addr() as u64
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: a handle this value owns, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// xorshift64*: reproducible choices from a seed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_u32(&mut self) -> u32 {
        u32::try_from(self.next() >> 32).unwrap_or(0)
    }

    /// A number below `bound` (at least 1).
    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }

    /// An index below `len` (0 when `len` is 0).
    fn index(&mut self, len: usize) -> usize {
        usize::try_from(self.below(len as u64)).unwrap_or(0)
    }

    /// A buffer size: small, around the requests' sizes, or large.
    fn size(&mut self) -> usize {
        const SIZES: [usize; 8] = [0, 1, 8, 16, 32, 40, 4096, BUFFER];
        SIZES[usize::try_from(self.below(SIZES.len() as u64)).unwrap_or(0)]
    }

    fn fill(&mut self, buffer: &mut [u8]) {
        for chunk in buffer.chunks_mut(8) {
            let bytes = self.next().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
}
