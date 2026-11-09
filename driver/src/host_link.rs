//! What the capture stream and the host interface share: the stream's state as the host sees
//! it, and the attached host (PLAN.md, M4).
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
//! The link lives in the adapter's device extension, whose memory outlives its holders: an
//! open pin keeps the adapter's device object alive until the stream is released; the host
//! interface's device refuses removal while a handle to it is open, so the PnP manager cannot
//! remove (and PortCls delete) the adapter before every request to the interface is over.

use core::{
    cell::UnsafeCell,
    ptr::NonNull,
    sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering},
};

use hostmic_abi::{Format, State, StreamState};
use hostmic_driver_core::host::Cursor;
use wdk::println;
use wdk_sys::{
    EX_RUNDOWN_REF, FILE_OBJECT, KEVENT,
    ntddk::{
        ExAcquireRundownProtection, ExReInitializeRundownProtection, ExReleaseRundownProtection,
        ExWaitForRundownProtectionRelease, KeSetEvent, ObfDereferenceObject,
    },
};

/// `HostLink::host` while a detachment is in progress: no file object has this address.
const DETACHING: *mut FILE_OBJECT = core::ptr::dangling_mut();
/// `HostLink::host` while an attachment is in progress, likewise.
const ATTACHING: *mut FILE_OBJECT = DETACHING.wrapping_add(1);

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
    /// The attached host's file object, compared by address only; null without a host.
    host: AtomicPtr<FILE_OBJECT>,
    /// The attached host's event, referenced; null without a host.
    event: AtomicPtr<KEVENT>,
    /// Keeps `event` alive while [`HostLink::notify`] signals it. All zeroes is an initialized
    /// rundown reference.
    rundown: UnsafeCell<EX_RUNDOWN_REF>,
}

// SAFETY: every field is atomic except the rundown reference, which the kernel's rundown
// routines manipulate atomically.
unsafe impl Sync for HostLink {}

impl HostLink {
    /// Sets the initial cursor (a stopped stream, odd generation) on the adapter's first start.
    pub fn init(&self) {
        let _ = self.cursor.compare_exchange(
            0,
            Cursor::STOPPED.pack(),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
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

    /// Signals the attached host's event, if any. Any IRQL up to `DISPATCH_LEVEL`; never waits.
    pub fn notify(&self) {
        // SAFETY: a valid rundown reference, initialized (all zeroes or reinitialized).
        if unsafe { ExAcquireRundownProtection(self.rundown.get()) } == 0 {
            return;
        }
        let event = self.event.load(Ordering::Acquire);
        if !event.is_null() {
            // SAFETY: a referenced event, kept alive by the rundown protection held here
            // (`detach` waits for it before dereferencing). No wait follows.
            unsafe { KeSetEvent(event, 0, 0) };
        }
        // SAFETY: acquired above.
        unsafe { ExReleaseRundownProtection(self.rundown.get()) };
    }

    /// Attaches the host behind `file`, which gave `event` (one reference, owned by the link
    /// on success). `PASSIVE_LEVEL`.
    ///
    /// # Errors
    ///
    /// `event`, given back, when another host is attached; the caller dereferences it.
    pub fn attach(&self, file: *mut FILE_OBJECT, event: *mut KEVENT) -> Result<(), *mut KEVENT> {
        // Claim the slot, store the event, then publish the file object: a `DETACH` or a
        // cleanup racing on the same handle cannot see `file` before the event is in place
        // (it fails, and the handle's close detaches it once this request is over).
        if file.is_null()
            || self
                .host
                .compare_exchange(
                    core::ptr::null_mut(),
                    ATTACHING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
        {
            return Err(event);
        }
        self.event.store(event, Ordering::Release);
        self.host.store(file, Ordering::Release);
        println!("hostmic: host attached");
        // The host reads the stream's state as it is now.
        self.notify();
        Ok(())
    }

    /// Detaches the host behind `file`, if it is the attached one; returns whether it was.
    /// `PASSIVE_LEVEL`: waits for a signal in flight before releasing the event.
    pub fn detach(&self, file: *mut FILE_OBJECT) -> bool {
        // Claim the detachment: a concurrent `DETACH` and handle cleanup on the same file
        // object cannot both get past this, and no host can attach until it is over.
        if file.is_null()
            || self
                .host
                .compare_exchange(file, DETACHING, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return false;
        }
        let event = self.event.swap(core::ptr::null_mut(), Ordering::AcqRel);
        // SAFETY: PASSIVE_LEVEL; waits for `notify` calls holding the protection.
        unsafe { ExWaitForRundownProtectionRelease(self.rundown.get()) };
        // SAFETY: the rundown completed and nobody holds it.
        unsafe { ExReInitializeRundownProtection(self.rundown.get()) };
        if !event.is_null() {
            // SAFETY: the reference `attach` took ownership of; nobody uses the event any more.
            unsafe { ObfDereferenceObject(event.cast()) };
        }
        self.host.store(core::ptr::null_mut(), Ordering::Release);
        println!("hostmic: host detached");
        true
    }
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
