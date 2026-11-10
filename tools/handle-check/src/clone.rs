//! `--clone`: does a clone of the host's address space keep the shared buffer?
//!
//! The driver maps the shared buffer into the host's process and undoes that mapping in the
//! host's process only. A host can clone its own address space after attaching: a process
//! snapshot with `PSS_CAPTURE_VA_CLONE`, or a fork (`RtlCloneUserProcess`, here created
//! suspended so that it never runs). If the clone kept the mapping, it would keep reading the
//! driver's pages after the host detaches: the next host's audio, and after the adapter's
//! removal, pages already freed.
//!
//! The check attaches, writes a pattern into the buffer, and makes both clones; it fails unless
//! both are made. Each clone must read two controls, a private allocation and a view of a
//! section, both written before cloning: so that the clone is known to be readable and to
//! carry this process's private and mapped memory. Then, with the host attached, after it
//! writes a second pattern, after `DETACH` and with a next attachment that writes a third,
//! the region at the buffer's address must not be committed in either clone, nor readable; and
//! no readable memory of either clone may hold the second or the third pattern, which were
//! written after cloning, so that only pages shared with the live buffer could show them.

use core::ffi::c_void;

use hostmic_abi::IOCTL_DETACH;
use windows::{
    Win32::{
        Foundation::{HANDLE, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::{
                Debug::ReadProcessMemory,
                ProcessSnapshotting::{
                    HPSS, PSS_CAPTURE_VA_CLONE, PSS_QUERY_VA_CLONE_INFORMATION,
                    PSS_VA_CLONE_INFORMATION, PssCaptureSnapshot, PssFreeSnapshot,
                    PssQuerySnapshot,
                },
            },
            Memory::{
                CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEM_COMMIT, MEM_FREE, MEM_IMAGE,
                MEM_MAPPED, MEM_PRIVATE, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION,
                MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_GUARD, PAGE_NOACCESS,
                PAGE_READWRITE, UnmapViewOfFile, VirtualAlloc, VirtualFree, VirtualQueryEx,
            },
            Threading::{GetCurrentProcess, TerminateProcess},
        },
    },
    core::PCWSTR,
};

use crate::{Handle, attach, first_interface};

/// Bytes of the buffer written and read back, and the length of a pattern's run.
const SPAN: usize = 256;
/// The pattern written while the first host is attached, before cloning.
const FIRST: u8 = 0xA1;
/// The pattern written by the same host after cloning.
const SECOND: u8 = 0xB2;
/// The pattern written by the next host.
const NEXT: u8 = 0xC3;
/// The private control's byte.
const PRIVATE_BYTE: u8 = 0x5A;
/// The mapped control's byte.
const MAPPED_BYTE: u8 = 0x6B;
/// Bytes of each control: one page.
const CONTROL_BYTES: usize = 4096;
/// The highest user-mode address scanned (x64).
const USER_END: usize = 0x7FFF_FFFF_0000;

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
        // SAFETY: the clone's process handle, valid until the snapshot is freed or the fork's
        // handle is closed; failing to terminate a VA clone (which has no thread) is harmless.
        let _ = unsafe { TerminateProcess(self.process(), 0) };
        if let Self::Snapshot { snapshot, .. } = self {
            // SAFETY: this process's snapshot, freed once (its VA clone handle with it).
            let _ = unsafe { PssFreeSnapshot(current(), *snapshot) };
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

/// The two controls: a private allocation and a view of a pagefile-backed section, each one
/// page of its own byte, released on drop.
struct Controls {
    private: *mut c_void,
    section: Handle,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}

impl Controls {
    fn new() -> Result<Self, String> {
        // SAFETY: a new private, committed, read-write page anywhere.
        let private = unsafe {
            VirtualAlloc(
                None,
                CONTROL_BYTES,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        if private.is_null() {
            return Err("VirtualAlloc failed".to_owned());
        }
        // SAFETY: a pagefile-backed, unnamed section of one page.
        let section = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                u32::try_from(CONTROL_BYTES).unwrap_or(0),
                PCWSTR::null(),
            )
        };
        let section = section.map(Handle).map_err(|e| {
            // SAFETY: the allocation above, released once.
            let _ = unsafe { VirtualFree(private, 0, MEM_RELEASE) };
            format!("CreateFileMappingW: {e}")
        })?;
        // SAFETY: the section above, mapped whole for reading and writing.
        let view = unsafe { MapViewOfFile(section.0, FILE_MAP_ALL_ACCESS, 0, 0, CONTROL_BYTES) };
        let controls = Self {
            private,
            section,
            view,
        };
        if view.Value.is_null() {
            return Err("MapViewOfFile failed".to_owned());
        }
        // SAFETY: both are one committed, writable page of this process.
        unsafe {
            private
                .cast::<u8>()
                .write_bytes(PRIVATE_BYTE, CONTROL_BYTES);
        };
        // SAFETY: as above.
        unsafe {
            view.Value
                .cast::<u8>()
                .write_bytes(MAPPED_BYTE, CONTROL_BYTES);
        };
        Ok(controls)
    }
}

impl Drop for Controls {
    fn drop(&mut self) {
        if !self.view.Value.is_null() {
            // SAFETY: the view mapped in `new`, unmapped once.
            let _ = unsafe { UnmapViewOfFile(self.view) };
        }
        // SAFETY: the allocation of `new`, released once.
        let _ = unsafe { VirtualFree(self.private, 0, MEM_RELEASE) };
        let _ = &self.section;
    }
}

/// Fills the first [`SPAN`] bytes of the buffer mapped at `address` in this process.
fn fill(address: u64, value: u8) -> Result<(), String> {
    let base = usize::try_from(address).map_err(|_| "address beyond usize".to_owned())?;
    for offset in 0..SPAN {
        let byte = core::ptr::with_exposed_provenance_mut::<u8>(base + offset);
        // SAFETY: the shared buffer, mapped writable into this process by `ATTACH`, still
        // attached, and at least `SPAN` bytes long (checked in `clone_check`); written
        // volatile, as the audio engine reads it.
        unsafe { byte.write_volatile(value) };
    }
    Ok(())
}

/// What a clone holds at an address.
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

/// Reads `bytes.len()` bytes of `process` at `address`; whether it could.
fn read_into(process: HANDLE, address: usize, bytes: &mut [u8]) -> bool {
    // SAFETY: a process handle with read access; a writable buffer of its own length.
    unsafe {
        ReadProcessMemory(
            process,
            core::ptr::with_exposed_provenance::<c_void>(address),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            None,
        )
    }
    .is_ok()
}

/// What `clone` holds at `address`.
fn read(clone: &Clone, address: usize) -> Seen {
    let mut bytes = [0u8; SPAN];
    if !read_into(clone.process(), address, &mut bytes) {
        Seen::Unreadable
    } else if bytes.iter().all(|&b| b == bytes[0]) {
        Seen::Byte(bytes[0])
    } else {
        Seen::Mixed
    }
}

/// The region of `process`'s address space containing `address`, if any.
fn query(process: HANDLE, address: usize) -> Option<MEMORY_BASIC_INFORMATION> {
    let mut info = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: a process handle with query access; a writable information block.
    let written = unsafe {
        VirtualQueryEx(
            process,
            Some(core::ptr::with_exposed_provenance::<c_void>(address)),
            &raw mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    (written != 0).then_some(info)
}

/// A region, described: state, type, protection, size.
fn describe(info: Option<MEMORY_BASIC_INFORMATION>) -> String {
    let Some(info) = info else {
        return "no region".to_owned();
    };
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
    format!(
        "{state}, {kind}, protection {:#x}, {} bytes",
        info.Protect.0, info.RegionSize
    )
}

/// The address of the first run of [`SPAN`] bytes equal to `byte` in `process`'s readable
/// committed memory, if any.
fn scan(process: HANDLE, byte: u8) -> Option<usize> {
    const CHUNK: usize = 64 * 1024;
    let mut chunk = vec![0u8; CHUNK];
    let mut address = 0usize;
    while address < USER_END {
        let info = query(process, address)?;
        let base = info.BaseAddress.addr();
        let end = base.saturating_add(info.RegionSize.max(1));
        let readable = info.State == MEM_COMMIT
            && (info.Protect.0 & (PAGE_NOACCESS.0 | PAGE_GUARD.0)) == 0
            && info.Protect.0 != 0;
        if readable {
            // The run may cross chunks: counted across the region.
            let mut run = 0usize;
            let mut at = base;
            while at < end {
                let length = CHUNK.min(end - at);
                if read_into(process, at, &mut chunk[..length]) {
                    for (offset, &value) in chunk[..length].iter().enumerate() {
                        run = if value == byte { run + 1 } else { 0 };
                        if run == SPAN {
                            return Some(at + offset + 1 - SPAN);
                        }
                    }
                } else {
                    run = 0;
                }
                at += length;
            }
        }
        address = end;
    }
    None
}

/// Checks each clone at one stage: the region at `address` neither committed nor readable, and
/// no readable memory holding a run of `pattern` (when given, a byte written after cloning).
fn stage(clones: &[Clone], address: usize, what: &str, pattern: Option<u8>) -> Vec<String> {
    let mut failures = Vec::new();
    for clone in clones {
        let region = query(clone.process(), address);
        let seen = read(clone, address);
        println!("{:<10} {what}: {seen} ({})", clone.name(), describe(region));
        if region.is_some_and(|info| info.State == MEM_COMMIT) || seen != Seen::Unreadable {
            failures.push(format!(
                "{}: the buffer's address is committed or readable {what}",
                clone.name()
            ));
        }
        if let Some(byte) = pattern
            && let Some(found) = scan(clone.process(), byte)
        {
            println!(
                "{:<10} {what}: {byte:#04x} x {SPAN} found at {found:#x}",
                clone.name()
            );
            failures.push(format!(
                "{}: holds {byte:#04x}, written after cloning, {what}",
                clone.name()
            ));
        }
    }
    failures
}

/// Both clones, which must read both controls.
fn make_clones(controls: &Controls) -> Result<Vec<Clone>, String> {
    let mut clones = Vec::new();
    for made in [snapshot(), fork()] {
        clones.push(made.map_err(|e| format!("a clone was not made: {e}"))?);
    }
    for clone in &clones {
        let private = read(clone, controls.private.addr());
        let mapped = read(clone, controls.view.Value.addr());
        println!(
            "{:<10} controls: private {private}, mapped {mapped}",
            clone.name()
        );
        if private != Seen::Byte(PRIVATE_BYTE) || mapped != Seen::Byte(MAPPED_BYTE) {
            return Err(format!(
                "{}: a control is not readable, the check proves nothing",
                clone.name()
            ));
        }
    }
    Ok(clones)
}

/// `--clone`.
pub fn clone_check() -> Result<(), String> {
    let path = first_interface()?;
    println!("interface  {path}");
    let device = Handle::open(&path).map_err(|e| format!("open: {e}"))?;
    let (reply, event) = attach(&device)?;
    if (reply.buffer_bytes as usize) < SPAN {
        return Err(format!("a buffer of {} bytes", reply.buffer_bytes));
    }
    let address = usize::try_from(reply.buffer).map_err(|_| "address beyond usize".to_owned())?;
    println!(
        "attached   buffer {address:#x}: {}",
        describe(query(current(), address))
    );
    fill(reply.buffer, FIRST)?;
    let controls = Controls::new()?;
    let clones = make_clones(&controls)?;

    let mut failures = stage(&clones, address, "attached, before the host writes", None);
    fill(reply.buffer, SECOND)?;
    failures.extend(stage(
        &clones,
        address,
        "attached, after the host wrote again",
        Some(SECOND),
    ));
    device
        .control(IOCTL_DETACH, &[], &mut [])
        .map_err(|e| format!("DETACH: {e}"))?;
    drop(event);
    println!("detached");
    failures.extend(stage(&clones, address, "after DETACH", None));

    let (next, next_event) = attach(&device)?;
    fill(next.buffer, NEXT)?;
    println!("attached   again, buffer {:#x}", next.buffer);
    failures.extend(stage(
        &clones,
        address,
        "with the next host attached",
        Some(NEXT),
    ));
    device
        .control(IOCTL_DETACH, &[], &mut [])
        .map_err(|e| format!("DETACH: {e}"))?;
    drop(next_event);
    println!("detached");
    drop(clones);
    drop(controls);
    println!("clones     released");
    if failures.is_empty() {
        println!("verdict    no clone maps the buffer or holds what was written after cloning");
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}
