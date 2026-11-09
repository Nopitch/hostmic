//! What the capture stream and the host interface share: the stream's state as the host sees
//! it, the shared buffer, and the attached host (PLAN.md, M4).
//!
//! The adapter owns one [`HostLink`], in its device extension. The capture stream (one at a
//! time) publishes its state, format and buffer geometry here; the host interface reads them for
//! `GET_STREAM`, records which handle is attached and which event to signal, and the stream
//! signals that event whenever its state changes.
//!
//! The stream's generation and position follow `hostmic_driver_core::host::Cursor`'s protocol:
//! a state change or a new buffer first halts the generation, then changes the rest. The writers
//! here are the stream's state changes and buffer allocations (`PASSIVE_LEVEL`, serialized by
//! PortCls); `ADVANCE` will join them with its compare-and-exchange.
//!
//! The link holds the adapter's reference to the shared buffer ([`crate::shared_buffer`]), and
//! the attached host's mapping of it. A user mapping must be undone in its process before the
//! process ends: on `DETACH`, on the handle's cleanup or close (in whatever process closes it,
//! attaching to the host's), and, for a host that dies while another process holds a duplicate
//! of its handle, from the process notification routine ([`driver_init`]), which runs in the
//! dying process before its address space goes. Attaching and detaching are serialized by a
//! guarded mutex, so that the notification routine returns only once the mapping is undone.
//!
//! The link lives in the adapter's device extension, whose memory outlives its holders: an
//! open pin keeps the adapter's device object alive until the stream is released; the host
//! interface's device refuses removal while a handle to it is open, so the PnP manager cannot
//! remove (and PortCls delete) the adapter before every request to the interface is over; the
//! process notification routine reaches links through a registry that the adapter's removal
//! leaves under the same lock.

use core::{
    cell::UnsafeCell,
    ffi::c_void,
    mem::MaybeUninit,
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering},
};

use crate::com::Unknown;
use crate::shared_buffer::BufferRef;
use crate::topology::PIN_MICROPHONE;
use hostmic_abi::{Format, State, StreamState};
use hostmic_driver_core::host::Cursor;
use hostmic_sys::{IPortEvents, IPortEventsVtbl, KSEVENT_PINCAPS_CHANGENOTIFICATIONS, guid};
use wdk::println;
use wdk_sys::{
    _KPROCESS, EX_RUNDOWN_REF, FILE_OBJECT, HANDLE, KAPC_STATE, KEVENT, KGUARDED_MUTEX, NT_SUCCESS,
    NTSTATUS, PEPROCESS, PPS_CREATE_NOTIFY_INFO, STATUS_DEVICE_BUSY, STATUS_INSUFFICIENT_RESOURCES,
    STATUS_SUCCESS,
    ntddk::{
        ExAcquireRundownProtection, ExReInitializeRundownProtection, ExReleaseRundownProtection,
        ExWaitForRundownProtectionRelease, IoGetCurrentProcess, KeAcquireGuardedMutex,
        KeInitializeGuardedMutex, KeReleaseGuardedMutex, KeSetEvent, KeStackAttachProcess,
        KeUnstackDetachProcess, ObfDereferenceObject, ObfReferenceObject,
        PsSetCreateProcessNotifyRoutineEx,
    },
};

/// Shared between the capture stream and the host interface. All zeroes is its initial state
/// (as in a fresh device extension) until [`HostLink::init`].
#[repr(C)]
pub struct HostLink {
    /// The packed [`Cursor`]: generation and position.
    cursor: AtomicU64,
    /// The stream's [`State`] code.
    state: AtomicU32,
    /// The stream's [`Format`], as its 8 encoded bytes; all zeroes without a stream.
    format: AtomicU64,
    /// Frames of the buffer the stream uses; 0 without a buffer.
    buffer_frames: AtomicU32,
    /// Frames per packet in event-driven mode, 0 in polling mode.
    frames_per_packet: AtomicU32,
    /// The attached host's file object, compared by address only; null without a host. Changed
    /// under `host_lock` only, with the fields below up to `mapped`.
    host: AtomicPtr<FILE_OBJECT>,
    /// The attached host's event, referenced; null without a host.
    event: AtomicPtr<KEVENT>,
    /// Keeps `event` alive while [`HostLink::notify`] signals it. All zeroes is an initialized
    /// rundown reference.
    rundown: UnsafeCell<EX_RUNDOWN_REF>,
    /// The attached host's process, referenced; null without a host.
    process: AtomicPtr<_KPROCESS>,
    /// The shared buffer's address in the host's process; null without a host.
    mapping: AtomicPtr<c_void>,
    /// The reference to the shared buffer that the host's mapping holds (a raw [`BufferRef`]).
    mapped: AtomicPtr<c_void>,
    /// Serializes attaching and detaching; initialized by [`HostLink::init`].
    host_lock: UnsafeCell<KGUARDED_MUTEX>,
    /// The adapter's reference to the shared buffer (a raw [`BufferRef`]); null before the first
    /// start and after the removal. Changed under `host_lock`.
    buffer: AtomicPtr<c_void>,
    /// Whether the link is in the registry the process notification routine reads.
    registered: AtomicBool,
    /// The topology port's `IPortEvents`, to report jack changes: one reference, owned by the
    /// link; null while no topology miniport is initialized. As in Microsoft's sysvad sample,
    /// which keeps one in its topology miniport: PortCls releases the port's miniport when the
    /// filters stop, which drops the miniport and with it this reference, so there is no lasting
    /// cycle.
    jack_events: AtomicPtr<IPortEvents>,
    /// Keeps `jack_events` usable while a jack event is generated.
    jack_rundown: UnsafeCell<EX_RUNDOWN_REF>,
    /// Serializes the changes of `jack_events`; initialized by [`HostLink::init`].
    jack_lock: UnsafeCell<KGUARDED_MUTEX>,
}

// SAFETY: every field is atomic except the rundown references, which the kernel's rundown
// routines manipulate atomically, and the guarded mutexes, which the kernel's mutex routines
// manipulate.
unsafe impl Sync for HostLink {}

/// Why `ATTACH` failed after its input was accepted.
pub struct AttachError {
    /// The status the request completes with.
    pub status: NTSTATUS,
    /// Why, for the log.
    pub reason: &'static str,
}

impl HostLink {
    /// On each start of the adapter (`PASSIVE_LEVEL`, before any filter exists). The first time:
    /// the initial cursor (a stopped stream, odd generation), the locks and the registration
    /// for the process notification routine. Every time: the shared buffer, unless the link has
    /// it already.
    ///
    /// # Errors
    ///
    /// `STATUS_INSUFFICIENT_RESOURCES` when the shared buffer cannot be allocated.
    pub fn init(&self) -> Result<(), NTSTATUS> {
        if self
            .cursor
            .compare_exchange(
                0,
                Cursor::STOPPED.pack(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            // SAFETY: the first start: nothing uses the locks yet.
            unsafe { KeInitializeGuardedMutex(self.jack_lock.get()) };
            // SAFETY: as above.
            unsafe { KeInitializeGuardedMutex(self.host_lock.get()) };
            self.registered.store(register(self), Ordering::Release);
        }
        with_lock(&self.host_lock, || {
            if self.buffer.load(Ordering::Acquire).is_null() {
                let buffer = BufferRef::allocate().ok_or(STATUS_INSUFFICIENT_RESOURCES)?;
                println!("hostmic: shared buffer, {} bytes", buffer.size());
                self.buffer.store(buffer.into_raw(), Ordering::Release);
            }
            Ok(())
        })
    }

    /// On the adapter's removal (`PASSIVE_LEVEL`): leaves the registry and releases the
    /// adapter's reference to the shared buffer (streams and a host mapping keep theirs).
    pub fn shutdown(&self) {
        if self.registered.swap(false, Ordering::AcqRel) {
            unregister(self);
        }
        let buffer = with_lock(&self.host_lock, || {
            self.buffer.swap(ptr::null_mut(), Ordering::AcqRel)
        });
        // SAFETY: the adapter's reference, taken out of the link above.
        drop(unsafe { BufferRef::from_raw(buffer) });
    }

    /// A new reference to the shared buffer, for a stream; `None` before the first start or
    /// after the removal. `PASSIVE_LEVEL`.
    pub fn buffer(&self) -> Option<BufferRef> {
        with_lock(&self.host_lock, || {
            let raw = self.buffer.load(Ordering::Acquire);
            // SAFETY: the adapter's reference, which cannot be released while the lock is held;
            // borrowed to clone it, then given back.
            let adapter = unsafe { BufferRef::from_raw(raw) }?;
            let clone = adapter.clone();
            let _ = adapter.into_raw();
            Some(clone)
        })
    }

    /// Moves the cursor with `change`, which must move the generation.
    fn update(&self, change: impl Fn(Cursor) -> Cursor) {
        let _ = self
            .cursor
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |word| {
                Some(change(Cursor::unpack(word)).pack())
            });
    }

    /// A stream was created in `format`.
    pub fn stream_opened(&self, format: Format) {
        self.update(Cursor::stopped);
        self.state.store(State::Stopped.code(), Ordering::Release);
        self.format
            .store(u64::from_le_bytes(format.to_bytes()), Ordering::Release);
        self.notify();
    }

    /// The stream was freed.
    pub fn stream_closed(&self) {
        self.update(Cursor::stopped);
        self.state.store(State::Stopped.code(), Ordering::Release);
        self.buffer_frames.store(0, Ordering::Release);
        self.frames_per_packet.store(0, Ordering::Release);
        self.format.store(0, Ordering::Release);
        self.notify();
    }

    /// The stream got a buffer of `frames` frames (0: freed), in packets of
    /// `frames_per_packet` frames (0: polling mode).
    pub fn set_buffer(&self, frames: u32, frames_per_packet: u32) {
        // Halt first, so that no request computed for the old buffer can commit.
        self.update(Cursor::halted);
        self.buffer_frames.store(frames, Ordering::Release);
        self.frames_per_packet
            .store(frames_per_packet, Ordering::Release);
        self.notify();
    }

    /// The stream changed state.
    pub fn set_state(&self, state: State) {
        match state {
            State::Running => {
                self.state.store(state.code(), Ordering::Release);
                self.update(Cursor::running);
            }
            State::Paused => {
                self.update(Cursor::halted);
                self.state.store(state.code(), Ordering::Release);
            }
            State::Stopped => {
                self.update(Cursor::stopped);
                self.state.store(state.code(), Ordering::Release);
            }
        }
        self.notify();
    }

    /// What `GET_STREAM` replies. The fields are read one by one, then the cursor again: a
    /// reply is retried while the generation moved meanwhile, so the buffer geometry belongs to
    /// the generation reported. Within one generation the state may still lag by one step (a
    /// stream about to run reports `Running` under the last odd generation); the host reads
    /// again on the event every change signals.
    pub fn snapshot(&self) -> StreamState {
        let mut state = StreamState::default();
        for _ in 0..4 {
            let before = self.cursor.load(Ordering::Acquire);
            let cursor = Cursor::unpack(before);
            state = StreamState {
                generation: u32::from(cursor.generation),
                state: self.state.load(Ordering::Acquire),
                format: Format::from_bytes(&self.format.load(Ordering::Acquire).to_le_bytes()),
                buffer_frames: self.buffer_frames.load(Ordering::Acquire),
                frames_per_packet: self.frames_per_packet.load(Ordering::Acquire),
                position: cursor.position,
            };
            let after = Cursor::unpack(self.cursor.load(Ordering::Acquire));
            if after.generation == cursor.generation {
                break;
            }
        }
        state
    }

    /// Whether a host is attached: the microphone's jack is connected.
    pub fn is_attached(&self) -> bool {
        !self.host.load(Ordering::Acquire).is_null()
    }

    /// The topology miniport was initialized: the link keeps `events`, a reference to its
    /// port's `IPortEvents`, to report jack changes, and releases the previous one, if any.
    /// `PASSIVE_LEVEL`.
    pub fn set_jack_events(&self, events: Unknown) {
        let events = events.into_raw().cast::<IPortEvents>();
        with_lock(&self.jack_lock, || {
            let old = self.jack_events.swap(events, Ordering::AcqRel);
            self.release_jack_events(old);
        });
    }

    /// The topology miniport that gave `events` goes away: the link releases it, unless a
    /// newer miniport's replaced it already. `PASSIVE_LEVEL`: waits for a jack event in flight.
    pub fn clear_jack_events(&self, events: *mut IPortEvents) {
        if events.is_null() {
            return;
        }
        with_lock(&self.jack_lock, || {
            if self
                .jack_events
                .compare_exchange(events, ptr::null_mut(), Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.release_jack_events(events);
            }
        });
    }

    /// Releases a reference `jack_events` no longer holds, once no jack event uses it.
    /// Holding the jack lock, at `PASSIVE_LEVEL`.
    fn release_jack_events(&self, old: *mut IPortEvents) {
        if old.is_null() {
            return;
        }
        // SAFETY: PASSIVE_LEVEL; waits for the callers of `with_jack_events`, which may still
        // use `old`.
        unsafe { ExWaitForRundownProtectionRelease(self.jack_rundown.get()) };
        // SAFETY: the rundown completed; the jack lock keeps another wait from overlapping.
        unsafe { ExReInitializeRundownProtection(self.jack_rundown.get()) };
        // SAFETY: the reference `set_jack_events` took ownership of, used by nobody now.
        drop(unsafe { Unknown::from_raw(old.cast()) });
    }

    /// Calls `f` with the topology port's events interface, if there is one, while the link's
    /// reference to it cannot be released.
    fn with_jack_events(&self, f: impl FnOnce(*mut IPortEvents, &IPortEventsVtbl)) {
        // SAFETY: a valid, initialized rundown reference (zeroed or reinitialized).
        if unsafe { ExAcquireRundownProtection(self.jack_rundown.get()) } == 0 {
            return;
        }
        let events = self.jack_events.load(Ordering::Acquire);
        if !events.is_null() {
            // SAFETY: a referenced interface, which `release_jack_events` only releases after
            // the rundown protection held here.
            let vtable = unsafe { (*events).lpVtbl };
            // SAFETY: an interface's vtable is valid for its lifetime.
            f(events, unsafe { &*vtable });
        }
        // SAFETY: acquired above.
        unsafe { ExReleaseRundownProtection(self.jack_rundown.get()) };
    }

    /// Tells the clients (the audio endpoint builder) that the microphone's jack changed.
    fn jack_changed(&self) {
        self.with_jack_events(|events, vtable| {
            if let Some(generate) = vtable.GenerateEventList {
                let mut set = guid::KSEVENTSETID_PinCapsChange;
                // SAFETY: a live interface; the set GUID outlives the call; a pin event on the
                // microphone pin, no node.
                unsafe {
                    generate(
                        events.cast(),
                        &raw mut set,
                        KSEVENT_PINCAPS_CHANGENOTIFICATIONS::KSEVENT_PINCAPS_JACKINFOCHANGE
                            .cast_unsigned(),
                        1,
                        PIN_MICROPHONE,
                        0,
                        u32::MAX,
                    );
                };
            }
        });
    }

    /// Signals the attached host's event, if any. Any IRQL up to `DISPATCH_LEVEL`; never waits.
    pub fn notify(&self) {
        // SAFETY: a valid rundown reference, initialized (all zeroes or reinitialized).
        if unsafe { ExAcquireRundownProtection(self.rundown.get()) } == 0 {
            return;
        }
        let event = self.event.load(Ordering::Acquire);
        if !event.is_null() {
            // SAFETY: a referenced event, kept alive by the rundown protection held here
            // (`release_host` waits for it before dereferencing). No wait follows.
            unsafe { KeSetEvent(event, 0, 0) };
        }
        // SAFETY: acquired above.
        unsafe { ExReleaseRundownProtection(self.rundown.get()) };
    }

    /// Attaches the host behind `file`, which gave `event` (one reference, which the link owns
    /// from now on, and releases on failure), and maps the shared buffer into its process.
    /// Returns the buffer's address in the host's process.
    ///
    /// # Errors
    ///
    /// Another host is attached, the process notification routine is not registered, or the
    /// buffer cannot be mapped.
    ///
    /// # Safety
    ///
    /// `PASSIVE_LEVEL`, in the context of the process that becomes the host (the requestor).
    pub unsafe fn attach(
        &self,
        file: *mut FILE_OBJECT,
        event: *mut KEVENT,
    ) -> Result<NonNull<c_void>, AttachError> {
        let result = with_lock(&self.host_lock, || {
            if !self.host.load(Ordering::Acquire).is_null() {
                return Err(AttachError {
                    status: STATUS_DEVICE_BUSY,
                    reason: "another host is attached",
                });
            }
            if file.is_null() || !self.registered.load(Ordering::Acquire) {
                return Err(AttachError {
                    status: STATUS_INSUFFICIENT_RESOURCES,
                    reason: "no process notification: the mapping could outlive the host",
                });
            }
            let raw = self.buffer.load(Ordering::Acquire);
            // SAFETY: the adapter's reference, kept while the lock is held; borrowed to clone.
            let Some(adapter) = (unsafe { BufferRef::from_raw(raw) }) else {
                return Err(AttachError {
                    status: STATUS_INSUFFICIENT_RESOURCES,
                    reason: "no shared buffer",
                });
            };
            let buffer = adapter.clone();
            let _ = adapter.into_raw();
            // What a previous host wrote is not this host's business.
            buffer.zero(buffer.size());
            // SAFETY: PASSIVE_LEVEL, in the host's process (the caller's contract); undone by
            // `release_host`, before the process ends (see the module documentation).
            let Some(address) = (unsafe { buffer.map_user() }) else {
                return Err(AttachError {
                    status: STATUS_INSUFFICIENT_RESOURCES,
                    reason: "the shared buffer could not be mapped",
                });
            };
            // SAFETY: any IRQL; the current process, the host's.
            let process = unsafe { IoGetCurrentProcess() };
            // SAFETY: a live process object; the reference is released by `release_host`.
            unsafe { ObfReferenceObject(process.cast()) };
            self.process.store(process, Ordering::Release);
            self.mapping.store(address.as_ptr(), Ordering::Release);
            self.mapped.store(buffer.into_raw(), Ordering::Release);
            self.event.store(event, Ordering::Release);
            self.host.store(file, Ordering::Release);
            Ok(address)
        });
        match result {
            Ok(address) => {
                println!("hostmic: host attached, buffer mapped at {address:p}");
                // The microphone is plugged in.
                self.jack_changed();
                // The host reads the stream's state as it is now.
                self.notify();
            }
            Err(_) => {
                // SAFETY: the reference the caller gave, which the link did not keep.
                unsafe { ObfDereferenceObject(event.cast()) };
            }
        }
        result
    }

    /// Detaches the host behind `file`, if it is the attached one; returns whether it was.
    /// `PASSIVE_LEVEL`, in any process: the mapping is undone in the host's.
    pub fn detach(&self, file: *mut FILE_OBJECT) -> bool {
        let detached = with_lock(&self.host_lock, || {
            if file.is_null() || self.host.load(Ordering::Acquire) != file {
                return false;
            }
            self.release_host();
            true
        });
        if detached {
            // The microphone is unplugged.
            self.jack_changed();
        }
        detached
    }

    /// The process notification routine saw `process` exit: detaches the host if it is that
    /// process's. Runs in the exiting process, before its address space goes.
    fn process_exited(&self, process: PEPROCESS) {
        let detached = with_lock(&self.host_lock, || {
            let host = self.process.load(Ordering::Acquire);
            if host.is_null() || host != process {
                return false;
            }
            println!("hostmic: the host's process exited with its handle still open elsewhere");
            self.release_host();
            true
        });
        if detached {
            self.jack_changed();
        }
    }

    /// Releases everything the attached host holds: its event, once no signal is in flight,
    /// its mapping, in its process, and its process. Holding `host_lock`, at `PASSIVE_LEVEL`.
    fn release_host(&self) {
        let event = self.event.swap(ptr::null_mut(), Ordering::AcqRel);
        // SAFETY: PASSIVE_LEVEL; waits for `notify` calls holding the protection.
        unsafe { ExWaitForRundownProtectionRelease(self.rundown.get()) };
        // SAFETY: the rundown completed and nobody holds it; the lock keeps another wait away.
        unsafe { ExReInitializeRundownProtection(self.rundown.get()) };
        if !event.is_null() {
            // SAFETY: the reference `attach` took ownership of; nobody uses the event any more.
            unsafe { ObfDereferenceObject(event.cast()) };
        }
        let process = self.process.swap(ptr::null_mut(), Ordering::AcqRel);
        let mapping = self.mapping.swap(ptr::null_mut(), Ordering::AcqRel);
        // SAFETY: the mapping's reference, taken out of the link.
        let buffer =
            unsafe { BufferRef::from_raw(self.mapped.swap(ptr::null_mut(), Ordering::AcqRel)) };
        if let (Some(buffer), Some(mapping)) = (&buffer, NonNull::new(mapping))
            && !process.is_null()
        {
            // SAFETY: a mapping `attach` made in `process`, which is referenced and has not
            // ended: its end runs `process_exited`, which needs the lock held here.
            unsafe { unmap_in(process, buffer, mapping) };
        }
        drop(buffer);
        if !process.is_null() {
            // SAFETY: the reference `attach` took.
            unsafe { ObfDereferenceObject(process.cast()) };
        }
        self.host.store(ptr::null_mut(), Ordering::Release);
        println!("hostmic: host detached");
    }
}

/// Undoes the host's mapping of `buffer` at `mapping`, in `process`'s context.
///
/// # Safety
///
/// `mapping` came from `buffer.map_user()` in `process`, which is referenced and still has its
/// address space; `PASSIVE_LEVEL`.
unsafe fn unmap_in(process: PEPROCESS, buffer: &BufferRef, mapping: NonNull<c_void>) {
    // SAFETY: any IRQL.
    if unsafe { IoGetCurrentProcess() } == process {
        // SAFETY: the caller's contract, in the mapping's process.
        unsafe { buffer.unmap_user(mapping) };
        return;
    }
    let mut apc = MaybeUninit::<KAPC_STATE>::uninit();
    // SAFETY: below DISPATCH_LEVEL; a referenced process; the state lives on this thread's
    // stack until the matching `KeUnstackDetachProcess`.
    unsafe { KeStackAttachProcess(process, apc.as_mut_ptr()) };
    // SAFETY: the caller's contract, attached to the mapping's process. Nothing else runs
    // between the attach and the detach.
    unsafe { buffer.unmap_user(mapping) };
    // SAFETY: the matching detach, with the state the attach filled.
    unsafe { KeUnstackDetachProcess(apc.as_mut_ptr()) };
}

/// Runs `f` holding `lock`, a guarded mutex. `PASSIVE_LEVEL` or `APC_LEVEL`; a control path,
/// never per audio period.
fn with_lock<R>(lock: &UnsafeCell<KGUARDED_MUTEX>, f: impl FnOnce() -> R) -> R {
    // SAFETY: an initialized guarded mutex (`init`, `driver_init`), at IRQL <= APC_LEVEL.
    unsafe { KeAcquireGuardedMutex(lock.get()) };
    let result = f();
    // SAFETY: acquired above by this thread.
    unsafe { KeReleaseGuardedMutex(lock.get()) };
    result
}

/// The links the process notification routine looks at, one per started adapter.
static LINKS: [AtomicPtr<HostLink>; 4] = [const { AtomicPtr::new(ptr::null_mut()) }; 4];

/// A guarded mutex in a static.
struct StaticMutex(UnsafeCell<KGUARDED_MUTEX>);

// SAFETY: the kernel's guarded mutex routines are what touch it.
unsafe impl Sync for StaticMutex {}

/// Serializes the registry's changes with the notification routine's reads, so that a link
/// leaves the registry only when no notification uses it. Initialized by [`driver_init`].
static REGISTRY_LOCK: StaticMutex = StaticMutex(UnsafeCell::new(
    // SAFETY: plain data, all zeroes until `KeInitializeGuardedMutex`.
    unsafe { MaybeUninit::zeroed().assume_init() },
));

/// Whether [`process_notify`] is registered.
static NOTIFY_REGISTERED: AtomicBool = AtomicBool::new(false);

/// In `DriverEntry`, before any adapter starts: the registry's lock and the process
/// notification routine. Without the routine the driver still loads, but refuses to map the
/// buffer into a host (`ATTACH` fails).
pub fn driver_init() {
    // SAFETY: before any adapter: nothing uses the lock yet.
    unsafe { KeInitializeGuardedMutex(REGISTRY_LOCK.0.get()) };
    // SAFETY: PASSIVE_LEVEL; a routine of this image, removed by `driver_exit` before unload.
    // The image is linked with /INTEGRITYCHECK, which the `Ex` routine requires.
    let status = unsafe { PsSetCreateProcessNotifyRoutineEx(Some(process_notify), 0) };
    NOTIFY_REGISTERED.store(NT_SUCCESS(status), Ordering::Release);
    if status != STATUS_SUCCESS {
        println!("hostmic: process notification not registered: {status:#010x}");
    }
}

/// In the driver's unload routine: removes the process notification routine, which waits for
/// the calls in flight ("the system also waits for all in-flight callback routines to complete
/// before returning").
pub fn driver_exit() {
    if NOTIFY_REGISTERED.swap(false, Ordering::AcqRel) {
        // SAFETY: PASSIVE_LEVEL; the routine registered by `driver_init`.
        let _ = unsafe { PsSetCreateProcessNotifyRoutineEx(Some(process_notify), 1) };
    }
}

/// Adds `link` to the registry; returns whether there was room and the notification routine
/// is registered (otherwise no host can attach to it).
fn register(link: &HostLink) -> bool {
    if !NOTIFY_REGISTERED.load(Ordering::Acquire) {
        return false;
    }
    let link = ptr::from_ref(link).cast_mut();
    with_lock(&REGISTRY_LOCK.0, || {
        LINKS.iter().any(|slot| {
            slot.compare_exchange(ptr::null_mut(), link, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        })
    })
}

/// Removes `link` from the registry; once this returns, the notification routine no longer
/// uses it.
fn unregister(link: &HostLink) {
    let link = ptr::from_ref(link).cast_mut();
    with_lock(&REGISTRY_LOCK.0, || {
        for slot in &LINKS {
            let _ =
                slot.compare_exchange(link, ptr::null_mut(), Ordering::AcqRel, Ordering::Acquire);
        }
    });
}

/// The process notification routine: when a process exits, any host it is the process of is
/// detached, its mapping undone while the process's address space still exists.
///
/// # Safety
///
/// Called by the kernel at `PASSIVE_LEVEL`, for an exiting process in its last thread's
/// context (`create_info` null) or for a process being created.
unsafe extern "C" fn process_notify(
    process: PEPROCESS,
    _process_id: HANDLE,
    create_info: PPS_CREATE_NOTIFY_INFO,
) {
    if !create_info.is_null() {
        return;
    }
    with_lock(&REGISTRY_LOCK.0, || {
        for slot in &LINKS {
            let link = slot.load(Ordering::Acquire);
            if !link.is_null() {
                // SAFETY: a registered link, which stays alive while it is in the registry and
                // the registry's lock is held.
                unsafe { &*link }.process_exited(process);
            }
        }
    });
}

/// A pointer to the adapter's [`HostLink`], held by the objects that the link outlives: the
/// wave miniport, its stream and the host interface's device.
#[derive(Clone, Copy)]
pub struct LinkRef(NonNull<HostLink>);

// SAFETY: a `HostLink` is `Sync`, and the pointer stays valid as long as its holders (see the
// module documentation).
unsafe impl Send for LinkRef {}
// SAFETY: as above.
unsafe impl Sync for LinkRef {}

impl LinkRef {
    /// Wraps the adapter's link.
    ///
    /// # Safety
    ///
    /// `link` points to the adapter's link, which outlives every holder of the result.
    #[must_use]
    pub unsafe fn new(link: NonNull<HostLink>) -> Self {
        Self(link)
    }

    /// The link.
    #[must_use]
    pub fn get(&self) -> &HostLink {
        // SAFETY: valid for as long as `self` exists (`new`'s contract), shared access only.
        unsafe { self.0.as_ref() }
    }
}
