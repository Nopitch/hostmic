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
//! PortCls), and `ADVANCE`, which moves the position with one compare-and-exchange and loses
//! to any of them.
//!
//! The link holds the adapter's reference to the shared buffer ([`crate::shared_buffer`]), and
//! the attached host's mapping of it. A user mapping must be undone in its process before the
//! process ends: on `DETACH`, on the handle's cleanup or close (in whatever process closes it,
//! attaching to the host's), and, for a host that dies while another process holds a duplicate
//! of its handle, from the process notification routine ([`driver_init`]), which runs in the
//! context of the dying process's last thread. Attaching and detaching are serialized by a
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
use hostmic_abi::{Format, Reason, State, Stats, StreamState};
use hostmic_driver_core::{
    clock,
    format::SAMPLE_RATE,
    host::{self, Cursor, Rejection},
    packet,
};
use hostmic_sys::{IPortEvents, IPortEventsVtbl, KSEVENT_PINCAPS_CHANGENOTIFICATIONS, guid};
use wdk::println;
use wdk_sys::{
    _KPROCESS, EX_RUNDOWN_REF, FILE_OBJECT, HANDLE, KAPC_STATE, KEVENT, KGUARDED_MUTEX,
    LARGE_INTEGER, NT_SUCCESS, NTSTATUS, PEPROCESS, PPS_CREATE_NOTIFY_INFO, STATUS_DEVICE_BUSY,
    STATUS_INSUFFICIENT_RESOURCES, STATUS_SUCCESS,
    ntddk::{
        ExAcquireRundownProtection, ExReInitializeRundownProtection, ExReleaseRundownProtection,
        ExWaitForRundownProtectionRelease, IoGetCurrentProcess, KeAcquireGuardedMutex,
        KeInitializeGuardedMutex, KeQueryPerformanceCounter, KeReleaseGuardedMutex, KeSetEvent,
        KeStackAttachProcess, KeUnstackDetachProcess, ObfDereferenceObject, ObfReferenceObject,
        PsSetCreateProcessNotifyRoutineEx,
    },
};

/// The current performance counter value. Any IRQL.
fn qpc() -> u64 {
    // SAFETY: any IRQL; no frequency requested.
    let now = unsafe { KeQueryPerformanceCounter(ptr::null_mut()) };
    // SAFETY: `QuadPart` is the whole value of the union.
    unsafe { now.QuadPart }.cast_unsigned()
}

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
    /// Whether the link is in the registry the process notification routine reads, and hosts
    /// may attach; cleared (under `host_lock`) by the adapter's removal.
    registered: AtomicBool,
    /// Whether [`HostLink::init`] has run once: the cursor, the locks and the registration are
    /// set up. Not derived from the cursor, whose packed value may be 0 again once the
    /// generation wraps.
    started: AtomicBool,
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
    /// The audio engine's notification event, which `ADVANCE` signals when a packet completes;
    /// null without one. Not referenced: the stream registers and unregisters it, and
    /// unregistering waits for `engine_rundown`.
    engine_event: AtomicPtr<KEVENT>,
    /// Keeps `engine_event` valid while an `ADVANCE` signals it.
    engine_rundown: UnsafeCell<EX_RUNDOWN_REF>,
    /// Performance counter ticks per second, read once by [`HostLink::init`].
    qpc_frequency: AtomicU64,
    /// The performance counter and the position of the last `ADVANCE`, from which packets are
    /// dated. Written one after the other, so a reader may pair one advance's value with the
    /// next one's: an error of at most one advance's frames.
    advance_qpc: AtomicU64,
    advance_position: AtomicU64,
    /// The counters [`HostLink::stats`] reports (`hostmic_abi::Stats`).
    frames_advanced: AtomicU64,
    packets_completed: AtomicU64,
    packets_skipped: AtomicU64,
    requests_refused: AtomicU64,
    /// The last refusal: request function (bits 0-15), reason (16-31), status (32-63).
    last_refusal: AtomicU64,
}

/// Why `ADVANCE` was refused.
pub enum AdvanceError {
    /// The request does not come from the attached host's handle and process.
    NotHost,
    /// The validation in `hostmic-driver-core` refused it.
    Rejected(Rejection),
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
        // PnP requests for the adapter are serialized: no other start or removal runs meanwhile.
        if !self.started.load(Ordering::Acquire) {
            // No filter, hence no stream, exists before the first start.
            self.cursor.store(Cursor::STOPPED.pack(), Ordering::Release);
            let mut frequency = LARGE_INTEGER::default();
            // SAFETY: any IRQL; `frequency` is a valid out-parameter.
            unsafe { KeQueryPerformanceCounter(&raw mut frequency) };
            // SAFETY: `QuadPart` is the whole value of the union.
            let frequency = unsafe { frequency.QuadPart }.cast_unsigned();
            self.qpc_frequency.store(frequency, Ordering::Relaxed);
            // SAFETY: the first start: nothing uses the locks yet.
            unsafe { KeInitializeGuardedMutex(self.jack_lock.get()) };
            // SAFETY: as above.
            unsafe { KeInitializeGuardedMutex(self.host_lock.get()) };
            self.registered.store(register(self), Ordering::Release);
            self.started.store(true, Ordering::Release);
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

    /// On the adapter's removal (`PASSIVE_LEVEL`): no host may attach any more, a host still
    /// attached is detached (its mapping undone in its process), the link leaves the registry,
    /// and the adapter's reference to the shared buffer is released (streams keep theirs).
    pub fn shutdown(&self) {
        // Never started (the start failed before `init`): nothing to undo, and the locks were
        // never initialized. PnP requests for the adapter are serialized, so `init` cannot run
        // meanwhile.
        if !self.started.load(Ordering::Acquire) {
            return;
        }
        let (detached, buffer) = with_lock(&self.host_lock, || {
            // `attach` checks this under the same lock.
            self.registered.store(false, Ordering::Release);
            let attached = !self.host.load(Ordering::Acquire).is_null();
            if attached {
                println!("hostmic: adapter removed with a host attached");
                self.release_host();
            }
            (
                attached,
                self.buffer.swap(ptr::null_mut(), Ordering::AcqRel),
            )
        });
        if detached {
            self.jack_changed();
        }
        unregister(self);
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

    /// The stream position, in frames since the stream started (stopped: 0). Any IRQL; reads
    /// one atomic.
    pub fn position(&self) -> u64 {
        Cursor::unpack(self.cursor.load(Ordering::Acquire)).position
    }

    /// `IOCTL_ADVANCE`: the attached host wrote frames; moves the position with one
    /// compare-and-exchange (`hostmic_driver_core::host::Cursor` has the protocol), signals the
    /// audio engine when packets complete, and returns the new position. `PASSIVE_LEVEL`, in
    /// the requesting thread; no lock.
    ///
    /// # Errors
    ///
    /// The request does not come from the attached host's handle and process, or its input
    /// does not match the stream.
    pub fn advance(&self, file: *mut FILE_OBJECT, input: &[u8]) -> Result<u64, AdvanceError> {
        // SAFETY: any IRQL; the current process.
        let current = unsafe { IoGetCurrentProcess() };
        if file.is_null()
            || self.host.load(Ordering::Acquire) != file
            || self.process.load(Ordering::Acquire) != current
        {
            return Err(AdvanceError::NotHost);
        }
        // Each failed exchange means that another writer moved the cursor: its generation or its
        // position changed, so the next validation refuses a request computed for the old one.
        // The loop therefore runs at most twice.
        let (from, to, per_packet) = loop {
            let word = self.cursor.load(Ordering::Acquire);
            // Read after the word: a new buffer halts the generation before it changes these,
            // so values newer than the word make the exchange below fail.
            let buffer_frames = self.buffer_frames.load(Ordering::Acquire);
            let per_packet = self.frames_per_packet.load(Ordering::Acquire);
            let next = host::advance(input, word, buffer_frames).map_err(AdvanceError::Rejected)?;
            if self
                .cursor
                .compare_exchange(word, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break (
                    Cursor::unpack(word).position,
                    Cursor::unpack(next).position,
                    per_packet,
                );
            }
        };
        self.advance_qpc.store(qpc(), Ordering::Relaxed);
        self.advance_position.store(to, Ordering::Release);
        self.frames_advanced.fetch_add(to - from, Ordering::Relaxed);
        let completed = packet::completed_between(from, to, u64::from(per_packet));
        if completed != 0 {
            self.packets_completed
                .fetch_add(completed, Ordering::Relaxed);
            self.signal_engine();
        }
        Ok(to)
    }

    /// Signals the audio engine's notification event, if one is registered. Never waits.
    fn signal_engine(&self) {
        // SAFETY: a valid rundown reference, initialized (all zeroes or reinitialized).
        if unsafe { ExAcquireRundownProtection(self.engine_rundown.get()) } == 0 {
            return;
        }
        let event = self.engine_event.load(Ordering::Acquire);
        if !event.is_null() {
            // SAFETY: a registered event, valid until `clear_engine_event`, which waits for the
            // rundown protection held here. No wait follows.
            unsafe { KeSetEvent(event, 0, 0) };
        }
        // SAFETY: acquired above.
        unsafe { ExReleaseRundownProtection(self.engine_rundown.get()) };
    }

    /// The stream registered the audio engine's notification event; `false` if another one is
    /// registered.
    pub fn set_engine_event(&self, event: *mut KEVENT) -> bool {
        self.engine_event
            .compare_exchange(ptr::null_mut(), event, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// The stream unregistered `event`: returns once no `ADVANCE` signals it any more.
    /// `PASSIVE_LEVEL`; PortCls serializes registrations, so no other wait overlaps.
    pub fn clear_engine_event(&self, event: *mut KEVENT) {
        if self
            .engine_event
            .compare_exchange(event, ptr::null_mut(), Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        // SAFETY: PASSIVE_LEVEL; waits for `signal_engine` calls holding the protection.
        unsafe { ExWaitForRundownProtectionRelease(self.engine_rundown.get()) };
        // SAFETY: the rundown completed and nobody holds it.
        unsafe { ExReInitializeRundownProtection(self.engine_rundown.get()) };
    }

    /// The performance counter value at which stream frame `frame` was written, estimated from
    /// the last `ADVANCE` (its time and position) at the stream's rate. Any IRQL; reads atomics.
    pub fn frame_qpc(&self, frame: u64) -> u64 {
        let position = self.advance_position.load(Ordering::Acquire);
        let at = self.advance_qpc.load(Ordering::Relaxed);
        let back = clock::ticks_for_frames(
            position.saturating_sub(frame),
            self.qpc_frequency.load(Ordering::Relaxed),
            SAMPLE_RATE,
        );
        at.saturating_sub(back)
    }

    /// `GetReadPacket` skipped `count` packets the host had overwritten. Any IRQL.
    pub fn packets_skipped(&self, count: u64) {
        self.packets_skipped.fetch_add(count, Ordering::Relaxed);
    }

    /// A request of control code function `request` was refused, for `reason`, with `status`.
    /// Any IRQL.
    pub fn refused(&self, request: u16, reason: Reason, status: NTSTATUS) {
        self.requests_refused.fetch_add(1, Ordering::Relaxed);
        let packed = u64::from(request)
            | u64::from(reason.code()) << 16
            | u64::from(status.cast_unsigned()) << 32;
        self.last_refusal.store(packed, Ordering::Relaxed);
    }

    /// What `IOCTL_GET_STATS` replies.
    pub fn stats(&self) -> Stats {
        let refusal = self.last_refusal.load(Ordering::Relaxed);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "unpacks the fields `refused` packed"
        )]
        Stats {
            frames_advanced: self.frames_advanced.load(Ordering::Relaxed),
            packets_completed: self.packets_completed.load(Ordering::Relaxed),
            packets_skipped: self.packets_skipped.load(Ordering::Relaxed),
            requests_refused: self.requests_refused.load(Ordering::Relaxed),
            last_request: refusal as u16,
            last_reason: (refusal >> 16) as u16,
            last_status: (refusal >> 32) as u32,
        }
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
            let raw = self.buffer.load(Ordering::Acquire);
            // SAFETY: the adapter's reference, kept while the lock is held; borrowed to clone.
            let Some(adapter) = (unsafe { BufferRef::from_raw(raw) }) else {
                return Err(AttachError {
                    status: STATUS_INSUFFICIENT_RESOURCES,
                    reason: "no shared buffer: the adapter is not started",
                });
            };
            if file.is_null() || !self.registered.load(Ordering::Acquire) {
                let _ = adapter.into_raw();
                return Err(AttachError {
                    status: STATUS_INSUFFICIENT_RESOURCES,
                    reason: "no process notification: the mapping could outlive the host",
                });
            }
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
    /// process's. Runs in the exiting process's last thread (see [`process_notify`]).
    fn process_exited(&self, process: PEPROCESS) {
        // Every other process's exit returns here, without the lock: only a thread of the host's
        // process stores it (`attach` requires the requestor to be the current process), and
        // when this runs, no other thread of `process` is left to store it.
        if self.process.load(Ordering::Acquire) != process {
            return;
        }
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
/// detached and its mapping undone.
///
/// The documentation says the routine runs "in the context of the last thread to exit from the
/// process", at `PASSIVE_LEVEL` in a critical region, and is called "typically when the last
/// thread within a process has terminated and the process address space is about to be
/// deleted": the unmapping relies on the address space still being there, which the VM test
/// observes (the host's mapping is undone without a crash); Driver Verifier is for M6.
///
/// Microsoft advises against synchronizing with other threads here. Every exit takes the
/// registry's lock, held only briefly by an adapter's start and removal, and returns before any
/// other lock unless the exiting process is the host's ([`HostLink::process_exited`]); the
/// host's own exit then takes its link's lock, which another thread holds at most for one
/// attach or detach (none of which waits on a process exit), and generates the jack event,
/// which signals events without waiting.
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
