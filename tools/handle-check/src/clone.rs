//! `--clone`: does a clone of the host's address space keep the shared buffer?
//!
//! The driver maps the shared buffer into the host's process and undoes that mapping in the
//! host's process only. A host can clone its own address space after attaching: a process
//! snapshot with `PSS_CAPTURE_VA_CLONE`, or a fork (`RtlCloneUserProcess`, here created
//! suspended so that it never runs). If Windows copied the mapping into the clone, the clone
//! would keep reading the driver's pages after the host detaches: the next host's audio, and
//! after the adapter's removal, pages already freed.
//!
//! The check attaches, writes a pattern into the buffer, clones, then reads each clone's memory
//! at the buffer's address (`ReadProcessMemory`): with the host attached, after the host writes
//! another pattern (whether the clone shares the pages or holds a copy), after `DETACH`, and
//! after a new attachment writes a third pattern into the buffer (whether the clone sees the
//! next host's data). It fails when a clone sees that third pattern.

use core::ffi::c_void;

use hostmic_abi::IOCTL_DETACH;
use windows::Win32::{
    Foundation::HANDLE,
    System::{
        Diagnostics::{
            Debug::ReadProcessMemory,
            ProcessSnapshotting::{
                HPSS, PSS_CAPTURE_VA_CLONE, PSS_QUERY_VA_CLONE_INFORMATION,
                PSS_VA_CLONE_INFORMATION, PssCaptureSnapshot, PssFreeSnapshot, PssQuerySnapshot,
            },
        },
        Memory::{
            MEM_COMMIT, MEM_FREE, MEM_IMAGE, MEM_MAPPED, MEM_PRIVATE, MEM_RESERVE,
            MEMORY_BASIC_INFORMATION, VirtualQueryEx,
        },
        Threading::{GetCurrentProcess, TerminateProcess},
    },
};

use crate::{Handle, attach, first_interface};

/// Bytes of the buffer written and read back.
const SPAN: usize = 256;
/// The pattern written while the first host is attached, before cloning.
const FIRST: u8 = 0xA1;
/// The pattern written by the same host after cloning.
const SECOND: u8 = 0xB2;
/// The pattern written by the next host.
const NEXT: u8 = 0xC3;
/// The control's byte.
const CONTROL_BYTE: u8 = 0x5A;
/// Ordinary data of this process, which each clone must be able to read: without it, an
/// unreadable buffer would only mean the clone cannot be read at all.
static CONTROL: [u8; SPAN] = [CONTROL_BYTE; SPAN];

/// `RTL_CLONE_PROCESS_FLAGS_CREATE_SUSPENDED`: the clone never runs.
const CLONE_SUSPENDED: u32 = 0x1;
/// `STATUS_PROCESS_CLONED`: returned in the clone, were it to run.
const STATUS_PROCESS_CLONED: i32 = 0x129;

/// `RTL_USER_PROCESS_INFORMATION` (x64), with room to spare for its image information.
#[repr(C)]
struct CloneInformation {
    length: u32,
    process: HANDLE,
    thread: HANDLE,
    client_id: [usize; 2],
    image: [u8; 128],
}

#[link(name = "ntdll")]
unsafe extern "system" {
    /// Forks the calling process (ntdll, not in the Windows SDK's headers).
    fn RtlCloneUserProcess(
        flags: u32,
        process_security: *mut c_void,
        thread_security: *mut c_void,
        debug_port: HANDLE,
        information: *mut CloneInformation,
    ) -> i32;
}

/// This process's pseudo-handle.
fn current() -> HANDLE {
    // SAFETY: no precondition; the pseudo-handle needs no closing.
    unsafe { GetCurrentProcess() }
}

/// A clone of this process's address space, released on drop.
enum Clone {
    /// A process snapshot's VA clone, freed with the snapshot.
    Snapshot { snapshot: HPSS, process: HANDLE },
    /// A suspended fork, terminated.
    Fork { process: Handle, _thread: Handle },
}

impl Clone {
    fn name(&self) -> &'static str {
        match self {
            Self::Snapshot { .. } => "va-clone",
            Self::Fork { .. } => "fork",
        }
    }

    fn process(&self) -> HANDLE {
        match self {
            Self::Snapshot { process, .. } => *process,
            Self::Fork { process, .. } => process.0,
        }
    }
}

impl Drop for Clone {
    fn drop(&mut self) {
        match self {
            Self::Snapshot { snapshot, .. } => {
                // SAFETY: this process's snapshot, freed once (its VA clone with it).
                let _ = unsafe { PssFreeSnapshot(current(), *snapshot) };
            }
            Self::Fork { process, .. } => {
                // SAFETY: the fork's process handle, which this value owns.
                let _ = unsafe { TerminateProcess(process.0, 0) };
            }
        }
    }
}

/// A process snapshot of this process with a VA clone.
fn snapshot() -> Result<Clone, String> {
    let mut snapshot = HPSS::default();
    // SAFETY: this process's pseudo-handle; a writable snapshot handle.
    let error =
        unsafe { PssCaptureSnapshot(current(), PSS_CAPTURE_VA_CLONE, None, &raw mut snapshot) };
    if error != 0 {
        return Err(format!("PssCaptureSnapshot: error {error}"));
    }
    let mut information = PSS_VA_CLONE_INFORMATION::default();
    // SAFETY: a snapshot taken above; a buffer of the class's structure and size.
    let error = unsafe {
        PssQuerySnapshot(
            snapshot,
            PSS_QUERY_VA_CLONE_INFORMATION,
            (&raw mut information).cast(),
            u32::try_from(size_of::<PSS_VA_CLONE_INFORMATION>()).unwrap_or(0),
        )
    };
    if error != 0 {
        // SAFETY: the snapshot taken above, freed once.
        let _ = unsafe { PssFreeSnapshot(current(), snapshot) };
        return Err(format!("PssQuerySnapshot: error {error}"));
    }
    Ok(Clone::Snapshot {
        snapshot,
        process: information.VaCloneHandle,
    })
}

/// A suspended fork of this process.
fn fork() -> Result<Clone, String> {
    let mut information = CloneInformation {
        length: u32::try_from(size_of::<CloneInformation>()).unwrap_or(0),
        process: HANDLE::default(),
        thread: HANDLE::default(),
        client_id: [0; 2],
        image: [0; 128],
    };
    // SAFETY: default security, no debug port, a writable information block larger than
    // ntdll's.
    let status = unsafe {
        RtlCloneUserProcess(
            CLONE_SUSPENDED,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            HANDLE::default(),
            &raw mut information,
        )
    };
    if status == STATUS_PROCESS_CLONED {
        // The clone, were it ever resumed: it ends at once.
        // SAFETY: this process's pseudo-handle.
        let _ = unsafe { TerminateProcess(current(), 0) };
    }
    if status != 0 {
        return Err(format!("RtlCloneUserProcess: status {status:#010x}"));
    }
    Ok(Clone::Fork {
        process: Handle(information.process),
        _thread: Handle(information.thread),
    })
}

/// Fills the first [`SPAN`] bytes of the buffer mapped at `address` in this process.
fn fill(address: u64, value: u8) -> Result<(), String> {
    let base = usize::try_from(address).map_err(|_| "address beyond usize".to_owned())?;
    for offset in 0..SPAN {
        let byte = core::ptr::with_exposed_provenance_mut::<u8>(base + offset);
        // SAFETY: the shared buffer, mapped writable into this process by `ATTACH` and still
        // attached; written volatile, as the audio engine reads it.
        unsafe { byte.write_volatile(value) };
    }
    Ok(())
}

/// What a clone holds at the buffer's address.
#[derive(PartialEq, Eq)]
enum Seen {
    /// Nothing readable there.
    Unreadable,
    /// [`SPAN`] bytes, not all equal.
    Mixed,
    /// [`SPAN`] times this byte.
    Byte(u8),
}

impl core::fmt::Display for Seen {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unreadable => write!(f, "unreadable"),
            Self::Mixed => write!(f, "mixed bytes"),
            Self::Byte(byte) => write!(f, "{byte:#04x} x {SPAN}"),
        }
    }
}

/// What `clone` holds at `address`.
fn read(clone: &Clone, address: u64) -> Seen {
    let mut bytes = [0u8; SPAN];
    let Ok(base) = usize::try_from(address) else {
        return Seen::Unreadable;
    };
    // SAFETY: a process handle with read access; a writable buffer of `SPAN` bytes.
    let result = unsafe {
        ReadProcessMemory(
            clone.process(),
            core::ptr::with_exposed_provenance::<c_void>(base),
            bytes.as_mut_ptr().cast(),
            SPAN,
            None,
        )
    };
    if result.is_err() {
        Seen::Unreadable
    } else if bytes.iter().all(|&b| b == bytes[0]) {
        Seen::Byte(bytes[0])
    } else {
        Seen::Mixed
    }
}

/// What `clone`'s address space holds at `address`: its region's state, type and protection.
fn region(clone: &Clone, address: u64) -> String {
    let Ok(base) = usize::try_from(address) else {
        return "address beyond usize".to_owned();
    };
    let mut info = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: a process handle with query access; a writable information block.
    let written = unsafe {
        VirtualQueryEx(
            clone.process(),
            Some(core::ptr::with_exposed_provenance::<c_void>(base)),
            &raw mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if written == 0 {
        return "VirtualQueryEx failed".to_owned();
    }
    let state = match info.State {
        MEM_COMMIT => "committed",
        MEM_RESERVE => "reserved",
        MEM_FREE => "free",
        _ => "unknown state",
    };
    let kind = match info.Type {
        MEM_PRIVATE => "private",
        MEM_MAPPED => "mapped",
        MEM_IMAGE => "image",
        _ => "no type",
    };
    format!("{state}, {kind}, protection {:#x}", info.Protect.0)
}

/// `--clone`.
pub fn clone_check() -> Result<(), String> {
    let path = first_interface()?;
    println!("interface  {path}");
    let device = Handle::open(&path).map_err(|e| format!("open: {e}"))?;
    let (reply, event) = attach(&device)?;
    let address = reply.buffer;
    println!("attached   buffer {address:#x}");
    fill(address, FIRST)?;

    let mut clones = Vec::new();
    for (name, made) in [("va-clone", snapshot()), ("fork", fork())] {
        match made {
            Ok(clone) => clones.push(clone),
            Err(error) => println!("{name:<10} not made: {error}"),
        }
    }
    let control = CONTROL.as_ptr().expose_provenance() as u64;
    for clone in &clones {
        let seen = read(clone, control);
        println!("{:<10} control (static data): {seen}", clone.name());
        if seen != Seen::Byte(CONTROL_BYTE) {
            return Err(format!(
                "{}: the control is not readable, the check proves nothing",
                clone.name()
            ));
        }
    }
    let report = |stage: &str, clones: &[Clone]| {
        for clone in clones {
            println!(
                "{:<10} {stage}: {} ({})",
                clone.name(),
                read(clone, address),
                region(clone, address)
            );
        }
    };
    report(&format!("attached, host wrote {FIRST:#04x}"), &clones);
    fill(address, SECOND)?;
    report(
        &format!("attached, host wrote {SECOND:#04x} after the clone"),
        &clones,
    );

    device
        .control(IOCTL_DETACH, &[], &mut [])
        .map_err(|e| format!("DETACH: {e}"))?;
    drop(event);
    println!("detached");
    report("after DETACH", &clones);

    let (next, next_event) = attach(&device)?;
    fill(next.buffer, NEXT)?;
    println!(
        "attached   again, buffer {:#x}, next host wrote {NEXT:#04x}",
        next.buffer
    );
    let leaks: Vec<&str> = clones
        .iter()
        .filter(|clone| read(clone, address) == Seen::Byte(NEXT))
        .map(Clone::name)
        .collect();
    report("with the next host attached", &clones);
    device
        .control(IOCTL_DETACH, &[], &mut [])
        .map_err(|e| format!("DETACH: {e}"))?;
    drop(next_event);
    println!("detached");
    drop(clones);
    println!("clones     released");
    if leaks.is_empty() {
        println!("verdict    no clone sees the next host's data");
        Ok(())
    } else {
        Err(format!(
            "the next host's data is visible in: {}",
            leaks.join(", ")
        ))
    }
}
