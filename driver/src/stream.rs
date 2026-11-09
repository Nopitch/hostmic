//! The capture stream: the buffer the audio engine reads, its position, and the test tone.
//!
//! Ported from `CMiniportWaveRTStream` of Microsoft's `simpleaudiosample`, with a different
//! real-time design (PLAN.md, M3): no spin lock.
//!
//! - The stream's only writer is its timer callback ([`tick`]), every millisecond while the
//!   stream runs. It computes the position from the performance counter (frames since RUN, a
//!   pure function of the time), writes the tone a little ahead of it, publishes how far it
//!   wrote with a `Release` store, and signals the notification event when a packet completes.
//! - `GetPosition` and `GetReadPacket` only read: the position from the performance counter,
//!   capped at what the timer published (`Acquire` load), so they never report frames that are
//!   not in the buffer yet.
//! - State changes, buffer allocation and event registration run at `PASSIVE_LEVEL`, serialized
//!   by PortCls; before they take away something the timer uses (the running state, the
//!   buffer, the event), they wait for any timer callback in flight to finish
//!   (`KeFlushQueuedDpcs`).

use core::{
    ffi::c_void,
    ptr::{self, NonNull},
    slice,
    sync::atomic::{AtomicI32, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

use hostmic_abi::{Format, SampleType, State};
use hostmic_driver_core::{
    clock,
    format::{CHANNELS, FRAME_BYTES, SAMPLE_RATE},
    packet, ring,
};
use hostmic_sys::{
    BOOL, IMiniportWaveRTInputStreamVtbl, IMiniportWaveRTStreamNotificationVtbl, IPortWaveRTStream,
    KSAUDIO_POSITION, KSRTAUDIO_HWLATENCY, KSRTAUDIO_HWREGISTER, KSSTATE, PKSAUDIO_POSITION,
    PKSDATAFORMAT, PPORTWAVERTSTREAM, guid,
};
use wdk_sys::{
    _MEMORY_CACHING_TYPE, DWORD, EX_TIMER_HIGH_RESOLUTION, FALSE, LARGE_INTEGER,
    MEMORY_CACHING_TYPE, NTSTATUS, PEX_TIMER, PHYSICAL_ADDRESS, PKEVENT, PMDL, PVOID,
    STATUS_DEVICE_NOT_READY, STATUS_INSUFFICIENT_RESOURCES, STATUS_INVALID_DEVICE_STATE,
    STATUS_INVALID_PARAMETER, STATUS_NOT_FOUND, STATUS_NOT_IMPLEMENTED, STATUS_NOT_SUPPORTED,
    STATUS_SUCCESS, STATUS_UNSUCCESSFUL, ULONG, ULONG64,
    ntddk::{
        ExAllocateTimer, ExCancelTimer, ExDeleteTimer, ExSetTimer, KeFlushQueuedDpcs,
        KeQueryPerformanceCounter, KeSetEvent,
    },
};

use crate::com::{self, ComObject, Unknown};
use crate::host_link::LinkRef;

/// Timer period: 1 ms, in 100 ns units (the sample's).
const TIMER_PERIOD: i64 = 10_000;

/// How far ahead of the position the timer writes: two timer periods, so that a late timer
/// callback does not stall the position.
const LEAD_FRAMES: u64 = 2 * SAMPLE_RATE as u64 / 1_000;

/// No packet read yet.
const NO_PACKET: u64 = u64::MAX;

/// `TRUE` and `FALSE` as the kernel's one-byte `BOOLEAN`.
const BOOLEAN_TRUE: u8 = 1;
const BOOLEAN_FALSE: u8 = 0;

/// The capture stream object.
#[repr(C)]
pub struct Stream {
    /// `IUnknown`, `IMiniportWaveRTStream` and `IMiniportWaveRTStreamNotification`, which
    /// extend one another.
    notification: &'static IMiniportWaveRTStreamNotificationVtbl,
    /// `IMiniportWaveRTInputStream` (`GetReadPacket`).
    input: &'static IMiniportWaveRTInputStreamVtbl,
    references: AtomicU32,
    /// The port's stream, which allocates the buffer. Not counted: it owns this stream.
    port: AtomicPtr<IPortWaveRTStream>,
    /// The timer, allocated right after the object (it needs the object's address).
    timer: AtomicPtr<wdk_sys::_EX_TIMER>,
    /// Performance counter ticks per second.
    qpc_frequency: AtomicU64,
    /// `KSSTATE`.
    state: AtomicI32,
    /// Performance counter value at stream frame 0 of the current run, adjusted on resume so
    /// that the position continues where the pause left it.
    start_qpc: AtomicU64,
    /// The position when not running.
    paused_frames: AtomicU64,
    /// Stream frames written into the buffer so far (by the timer only).
    written: AtomicU64,
    /// Packets the timer has signaled (timer only).
    signaled_packets: AtomicU64,
    /// Last packet number `GetReadPacket` returned.
    last_read_packet: AtomicU64,
    /// The buffer, mapped in system space, and its size in bytes (whole frames).
    buffer: AtomicPtr<u8>,
    buffer_bytes: AtomicUsize,
    /// Frames per packet in event-driven mode, 0 in polling mode.
    frames_per_packet: AtomicU64,
    /// The notification event of event-driven mode.
    event: AtomicPtr<wdk_sys::_KEVENT>,
    /// The adapter's host link, where the stream publishes its state for the host.
    link: LinkRef,
}

/// Offset of the `IMiniportWaveRTInputStream` vtable field.
const INPUT_OFFSET: usize = core::mem::offset_of!(Stream, input);

// SAFETY: `#[repr(C)]`; the vtables at offsets 0 and `INPUT_OFFSET` are `NOTIFICATION_VTABLE`
// and `INPUT_VTABLE`, whose `IUnknown` entries are the generic ones for this type and those
// offsets; all state is atomic.
unsafe impl ComObject for Stream {
    const INTERFACES: &'static [(wdk_sys::GUID, usize)] = &[
        (guid::IID_IMiniportWaveRTStream, 0),
        (guid::IID_IMiniportWaveRTStreamNotification, 0),
        (guid::IID_IMiniportWaveRTInputStream, INPUT_OFFSET),
    ];

    fn references(&self) -> &AtomicU32 {
        &self.references
    }
}

impl Drop for Stream {
    /// Runs at `PASSIVE_LEVEL`, from PortCls's last `Release`.
    fn drop(&mut self) {
        let timer = self.timer.load(Ordering::Acquire);
        if !timer.is_null() {
            // SAFETY: `timer` was allocated by `ExAllocateTimer` and is deleted once; cancel it
            // and wait for a callback in flight, which uses this object, to finish.
            unsafe { ExDeleteTimer(timer, BOOLEAN_TRUE, BOOLEAN_TRUE, ptr::null_mut()) };
        }
        // SAFETY: PASSIVE_LEVEL; waits for any DPC still queued.
        unsafe { KeFlushQueuedDpcs() };
        self.link.get().stream_closed();
    }
}

/// Creates a capture stream bound to `port_stream`, or `None` when the pool is exhausted.
///
/// # Safety
///
/// `port_stream` is the port's stream for the new stream, valid for its whole life.
pub unsafe fn new(port_stream: PPORTWAVERTSTREAM, link: LinkRef) -> Option<Unknown> {
    let mut frequency = LARGE_INTEGER::default();
    // SAFETY: any IRQL; `frequency` is a valid out-parameter.
    unsafe { KeQueryPerformanceCounter(&raw mut frequency) };
    // SAFETY: `QuadPart` is the whole value of the union.
    let frequency = unsafe { frequency.QuadPart }.cast_unsigned();

    let stream = com::into_unknown(Stream {
        notification: &NOTIFICATION_VTABLE,
        input: &INPUT_VTABLE,
        references: AtomicU32::new(1),
        port: AtomicPtr::new(port_stream),
        timer: AtomicPtr::new(ptr::null_mut()),
        qpc_frequency: AtomicU64::new(frequency),
        state: AtomicI32::new(KSSTATE::KSSTATE_STOP),
        start_qpc: AtomicU64::new(0),
        paused_frames: AtomicU64::new(0),
        written: AtomicU64::new(0),
        signaled_packets: AtomicU64::new(0),
        last_read_packet: AtomicU64::new(NO_PACKET),
        buffer: AtomicPtr::new(ptr::null_mut()),
        buffer_bytes: AtomicUsize::new(0),
        frames_per_packet: AtomicU64::new(0),
        event: AtomicPtr::new(ptr::null_mut()),
        link,
    })?;
    let object = stream.as_ptr().cast::<Stream>();
    // SAFETY: `tick` matches `EXT_CALLBACK`; its context is the stream, which deletes the
    // timer (waiting for callbacks) before it is freed.
    let timer = unsafe { ExAllocateTimer(Some(tick), object.cast(), EX_TIMER_HIGH_RESOLUTION) };
    if timer.is_null() {
        return None; // `stream` is released here.
    }
    // SAFETY: `object` is the live stream `stream` owns.
    unsafe { &*object }.timer.store(timer, Ordering::Release);
    link.get().stream_opened(Format {
        rate: SAMPLE_RATE,
        channels: CHANNELS,
        sample_type: SampleType::Int16.code(),
    });
    Some(stream)
}

impl Stream {
    /// The current performance counter value.
    fn qpc() -> u64 {
        // SAFETY: any IRQL; no frequency requested.
        let now = unsafe { KeQueryPerformanceCounter(ptr::null_mut()) };
        // SAFETY: `QuadPart` is the whole value of the union.
        unsafe { now.QuadPart }.cast_unsigned()
    }

    /// Frames since stream frame 0 of the current run, at performance counter value `now`.
    fn elapsed(&self, now: u64) -> u64 {
        let start = self.start_qpc.load(Ordering::Acquire);
        clock::frames_elapsed(
            now.saturating_sub(start),
            self.qpc_frequency.load(Ordering::Relaxed),
            SAMPLE_RATE,
        )
    }

    /// The stream position in frames: elapsed time while running, the paused position
    /// otherwise; never beyond what the timer has written.
    fn position(&self) -> u64 {
        let written = self.written.load(Ordering::Acquire);
        let position = if self.state.load(Ordering::Acquire) == KSSTATE::KSSTATE_RUN {
            self.elapsed(Self::qpc())
        } else {
            self.paused_frames.load(Ordering::Relaxed)
        };
        position.min(written)
    }

    /// Stops the timer and waits for a callback in flight. `PASSIVE_LEVEL`.
    fn stop_timer(&self) {
        let timer = self.timer.load(Ordering::Acquire);
        if !timer.is_null() {
            // SAFETY: a live timer of this stream.
            unsafe { ExCancelTimer(timer, ptr::null_mut()) };
        }
        // SAFETY: PASSIVE_LEVEL; waits for a timer callback already running or queued.
        unsafe { KeFlushQueuedDpcs() };
    }

    /// Allocates and maps the buffer; `frames_per_packet` is 0 in polling mode.
    fn allocate(
        &self,
        requested: ULONG,
        packets: ULONG,
        mdl_out: *mut PMDL,
        actual_out: *mut ULONG,
        offset_out: *mut ULONG,
        cache_out: *mut MEMORY_CACHING_TYPE,
    ) -> NTSTATUS {
        wdk::println!("hostmic: allocate {requested} bytes, {packets} packets");
        if mdl_out.is_null() || actual_out.is_null() || offset_out.is_null() || cache_out.is_null()
        {
            return STATUS_INVALID_PARAMETER;
        }
        let frames = usize::try_from(requested).unwrap_or(0) / FRAME_BYTES;
        if frames == 0 || !self.buffer.load(Ordering::Acquire).is_null() {
            return STATUS_UNSUCCESSFUL;
        }
        let bytes = frames * FRAME_BYTES;
        let Ok(bytes_ulong) = ULONG::try_from(bytes) else {
            return STATUS_INVALID_PARAMETER;
        };
        let port_stream = self.port.load(Ordering::Relaxed);
        // Below 4 GiB, as in the sample; nothing does DMA to it.
        let high = PHYSICAL_ADDRESS {
            QuadPart: 0xFFFF_FFFF,
        };
        // SAFETY: `port_stream` is the live port stream (`new`'s contract).
        let Some(mdl) = (unsafe { port_stream_call::allocate_pages(port_stream, high, bytes) })
        else {
            return STATUS_INSUFFICIENT_RESOURCES;
        };
        // SAFETY: `mdl` was just allocated by this port stream.
        let base = unsafe { port_stream_call::map(port_stream, mdl) };
        let Some(base) = NonNull::new(base.cast::<u8>()) else {
            // SAFETY: `mdl` was allocated above and is not mapped.
            unsafe { port_stream_call::free_pages(port_stream, mdl) };
            return STATUS_INSUFFICIENT_RESOURCES;
        };
        // SAFETY: `base` maps `bytes` bytes, not shared with the audio engine yet.
        unsafe { base.write_bytes(0, bytes) };

        self.frames_per_packet.store(
            if packets == 0 {
                0
            } else {
                u64::try_from(frames).unwrap_or(0) / u64::from(packets)
            },
            Ordering::Relaxed,
        );
        self.buffer_bytes.store(bytes, Ordering::Relaxed);
        self.buffer.store(base.as_ptr(), Ordering::Release);
        self.link.get().set_buffer(
            u32::try_from(frames).unwrap_or(u32::MAX),
            u32::try_from(self.frames_per_packet.load(Ordering::Relaxed)).unwrap_or(u32::MAX),
        );
        // SAFETY: the out-parameters are writable (checked not null above).
        unsafe { mdl_out.write(mdl) };
        // SAFETY: as above.
        unsafe { actual_out.write(bytes_ulong) };
        // SAFETY: as above.
        unsafe { offset_out.write(0) };
        // SAFETY: as above.
        unsafe { cache_out.write(_MEMORY_CACHING_TYPE::MmCached) };
        STATUS_SUCCESS
    }

    /// Unmaps and frees the buffer. `PASSIVE_LEVEL`, stream stopped.
    fn free(&self, mdl: PMDL) {
        let base = self.buffer.swap(ptr::null_mut(), Ordering::AcqRel);
        self.buffer_bytes.store(0, Ordering::Relaxed);
        self.frames_per_packet.store(0, Ordering::Relaxed);
        // The timer may no longer touch the buffer.
        self.stop_timer();
        self.link.get().set_buffer(0, 0);
        if mdl.is_null() {
            return;
        }
        let port_stream = self.port.load(Ordering::Relaxed);
        if !base.is_null() {
            // SAFETY: `base` is the mapping of `mdl` made in `allocate`.
            unsafe { port_stream_call::unmap(port_stream, base.cast(), mdl) };
        }
        // SAFETY: `mdl` was allocated by this port stream and is no longer mapped.
        unsafe { port_stream_call::free_pages(port_stream, mdl) };
    }

    /// `SetState`. `PASSIVE_LEVEL`, serialized by PortCls.
    fn set_state(&self, state: KSSTATE::Type) -> NTSTATUS {
        wdk::println!("hostmic: SetState {state}");
        let previous = self.state.load(Ordering::Acquire);
        match state {
            KSSTATE::KSSTATE_RUN => {
                if previous != KSSTATE::KSSTATE_RUN {
                    let paused = self.paused_frames.load(Ordering::Relaxed);
                    let back = clock::ticks_for_frames(
                        paused,
                        self.qpc_frequency.load(Ordering::Relaxed),
                        SAMPLE_RATE,
                    );
                    self.start_qpc
                        .store(Self::qpc().saturating_sub(back), Ordering::Release);
                    self.state.store(state, Ordering::Release);
                    let timer = self.timer.load(Ordering::Acquire);
                    // SAFETY: a live timer of this stream; relative due time, 1 ms period, no
                    // parameters (optional).
                    unsafe { ExSetTimer(timer, -TIMER_PERIOD, TIMER_PERIOD, ptr::null_mut()) };
                    self.link.get().set_state(State::Running);
                }
            }
            KSSTATE::KSSTATE_PAUSE | KSSTATE::KSSTATE_ACQUIRE => {
                if previous == KSSTATE::KSSTATE_RUN {
                    let position = self.position();
                    self.paused_frames.store(position, Ordering::Relaxed);
                    self.state.store(state, Ordering::Release);
                    self.stop_timer();
                    self.link.get().set_state(State::Paused);
                } else {
                    self.state.store(state, Ordering::Release);
                    if previous == KSSTATE::KSSTATE_STOP {
                        self.link.get().set_state(State::Paused);
                    }
                }
            }
            KSSTATE::KSSTATE_STOP => {
                self.state.store(state, Ordering::Release);
                self.stop_timer();
                // A stopped stream starts again from position 0.
                self.paused_frames.store(0, Ordering::Relaxed);
                self.written.store(0, Ordering::Release);
                self.signaled_packets.store(0, Ordering::Relaxed);
                self.last_read_packet.store(NO_PACKET, Ordering::Relaxed);
                if previous != KSSTATE::KSSTATE_STOP {
                    self.link.get().set_state(State::Stopped);
                }
            }
            _ => return STATUS_INVALID_PARAMETER,
        }
        STATUS_SUCCESS
    }
}

/// The timer callback, every millisecond while the stream runs: the stream's only writer.
///
/// Real-time path: no allocation, no lock, no I/O.
///
/// # Safety
///
/// Called by the kernel at `DISPATCH_LEVEL` with the stream as context; the stream deletes the
/// timer (waiting for callbacks) before it is freed.
unsafe extern "C" fn tick(_timer: PEX_TIMER, context: PVOID) {
    // SAFETY: the context is the live stream (see above).
    let stream = unsafe { &*context.cast::<Stream>() };
    if stream.state.load(Ordering::Acquire) != KSSTATE::KSSTATE_RUN {
        return;
    }
    let base = stream.buffer.load(Ordering::Acquire);
    if base.is_null() {
        return;
    }
    let bytes = stream.buffer_bytes.load(Ordering::Relaxed);
    let elapsed = stream.elapsed(Stream::qpc());

    let written = stream.written.load(Ordering::Relaxed);
    let target = elapsed.saturating_add(LEAD_FRAMES);
    if target > written {
        // SAFETY: `base` maps `bytes` bytes until `free`, which clears `buffer` and waits for
        // this callback before unmapping. This callback is the only writer; the audio engine
        // reads the same pages through its own mapping, outside Rust's memory model, like a
        // device's DMA.
        let ring = unsafe { slice::from_raw_parts_mut(base, bytes) };
        ring::write_tone(ring, written, target);
        stream.written.store(target, Ordering::Release);
    }

    let per_packet = stream.frames_per_packet.load(Ordering::Relaxed);
    if per_packet == 0 {
        return;
    }
    let complete = elapsed / per_packet;
    if complete > stream.signaled_packets.load(Ordering::Relaxed) {
        stream.signaled_packets.store(complete, Ordering::Relaxed);
        let event = stream.event.load(Ordering::Acquire);
        if !event.is_null() {
            // SAFETY: a registered event stays valid until `UnregisterNotificationEvent`, which
            // clears `event` and waits for this callback; DISPATCH_LEVEL is allowed without
            // waiting.
            unsafe { KeSetEvent(event, 0, BOOLEAN_FALSE) };
        }
    }
}

/// The stream of an interface pointer at offset 0.
///
/// # Safety
///
/// `this` is an interface pointer of a live stream, at offset 0.
unsafe fn this<'a>(this: *mut c_void) -> &'a Stream {
    // SAFETY: the caller's contract.
    unsafe { com::object::<Stream, 0>(this) }
}

static NOTIFICATION_VTABLE: IMiniportWaveRTStreamNotificationVtbl =
    IMiniportWaveRTStreamNotificationVtbl {
        QueryInterface: Some(com::query_interface::<Stream, 0>),
        AddRef: Some(com::add_ref::<Stream, 0>),
        Release: Some(com::release::<Stream, 0>),
        SetFormat: Some(set_format),
        SetState: Some(set_state),
        GetPosition: Some(get_position),
        AllocateAudioBuffer: Some(allocate_audio_buffer),
        FreeAudioBuffer: Some(free_audio_buffer),
        GetHWLatency: Some(get_hw_latency),
        GetPositionRegister: Some(get_register),
        GetClockRegister: Some(get_register),
        AllocateBufferWithNotification: Some(allocate_buffer_with_notification),
        FreeBufferWithNotification: Some(free_audio_buffer),
        RegisterNotificationEvent: Some(register_notification_event),
        UnregisterNotificationEvent: Some(unregister_notification_event),
    };

static INPUT_VTABLE: IMiniportWaveRTInputStreamVtbl = IMiniportWaveRTInputStreamVtbl {
    QueryInterface: Some(com::query_interface::<Stream, INPUT_OFFSET>),
    AddRef: Some(com::add_ref::<Stream, INPUT_OFFSET>),
    Release: Some(com::release::<Stream, INPUT_OFFSET>),
    GetReadPacket: Some(get_read_packet),
};

/// `SetFormat`: the format is fixed.
unsafe extern "C" fn set_format(_this: *mut c_void, _format: PKSDATAFORMAT) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

unsafe extern "C" fn set_state(this_: *mut c_void, state: KSSTATE::Type) -> NTSTATUS {
    // SAFETY: called through the stream's primary vtable.
    unsafe { this(this_) }.set_state(state)
}

/// `GetPosition`: byte offsets in the buffer. Any IRQL up to `DISPATCH_LEVEL`; reads only.
unsafe extern "C" fn get_position(this_: *mut c_void, position: PKSAUDIO_POSITION) -> NTSTATUS {
    if position.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: called through the stream's primary vtable.
    let stream = unsafe { this(this_) };
    let frames_in_buffer = stream.buffer_bytes.load(Ordering::Relaxed) / FRAME_BYTES;
    let offset = if frames_in_buffer == 0 {
        0
    } else {
        let frame = stream.position() % u64::try_from(frames_in_buffer).unwrap_or(u64::MAX);
        frame * FRAME_BYTES as u64
    };
    // SAFETY: `position` is writable (checked not null above).
    unsafe {
        position.write(KSAUDIO_POSITION {
            PlayOffset: offset,
            WriteOffset: offset,
        });
    };
    STATUS_SUCCESS
}

unsafe extern "C" fn allocate_audio_buffer(
    this_: *mut c_void,
    requested: ULONG,
    mdl: *mut PMDL,
    actual: *mut ULONG,
    offset: *mut ULONG,
    cache: *mut MEMORY_CACHING_TYPE,
) -> NTSTATUS {
    // SAFETY: called through the stream's primary vtable.
    unsafe { this(this_) }.allocate(requested, 0, mdl, actual, offset, cache)
}

unsafe extern "C" fn allocate_buffer_with_notification(
    this_: *mut c_void,
    notifications: ULONG,
    requested: ULONG,
    mdl: *mut PMDL,
    actual: *mut ULONG,
    offset: *mut ULONG,
    cache: *mut MEMORY_CACHING_TYPE,
) -> NTSTATUS {
    if notifications == 0 || !requested.is_multiple_of(notifications) {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: called through the stream's primary vtable.
    unsafe { this(this_) }.allocate(requested, notifications, mdl, actual, offset, cache)
}

/// `FreeAudioBuffer` and `FreeBufferWithNotification`.
unsafe extern "C" fn free_audio_buffer(this_: *mut c_void, mdl: PMDL, _size: ULONG) {
    // SAFETY: called through the stream's primary vtable.
    unsafe { this(this_) }.free(mdl);
}

/// `GetHWLatency`: no hardware, no latency.
unsafe extern "C" fn get_hw_latency(_this: *mut c_void, latency: *mut KSRTAUDIO_HWLATENCY) {
    if !latency.is_null() {
        // SAFETY: `latency` is writable (checked not null).
        unsafe {
            latency.write(KSRTAUDIO_HWLATENCY {
                FifoSize: 0,
                ChipsetDelay: 0,
                CodecDelay: 0,
            });
        };
    }
}

/// `GetPositionRegister` and `GetClockRegister`: no hardware registers.
unsafe extern "C" fn get_register(
    _this: *mut c_void,
    _register: *mut KSRTAUDIO_HWREGISTER,
) -> NTSTATUS {
    STATUS_NOT_IMPLEMENTED
}

/// `RegisterNotificationEvent`: one event, the audio engine's.
unsafe extern "C" fn register_notification_event(this_: *mut c_void, event: PKEVENT) -> NTSTATUS {
    if event.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: called through the stream's primary vtable.
    let stream = unsafe { this(this_) };
    match stream
        .event
        .compare_exchange(ptr::null_mut(), event, Ordering::AcqRel, Ordering::Acquire)
    {
        Ok(_) => STATUS_SUCCESS,
        Err(_) => STATUS_UNSUCCESSFUL,
    }
}

/// `UnregisterNotificationEvent`. `PASSIVE_LEVEL`.
unsafe extern "C" fn unregister_notification_event(this_: *mut c_void, event: PKEVENT) -> NTSTATUS {
    // SAFETY: called through the stream's primary vtable.
    let stream = unsafe { this(this_) };
    match stream
        .event
        .compare_exchange(event, ptr::null_mut(), Ordering::AcqRel, Ordering::Acquire)
    {
        Ok(_) => {
            // The caller may free the event when this returns: wait for a timer callback
            // that may still be signaling it.
            // SAFETY: PASSIVE_LEVEL.
            unsafe { KeFlushQueuedDpcs() };
            STATUS_SUCCESS
        }
        Err(_) => STATUS_NOT_FOUND,
    }
}

/// `IMiniportWaveRTInputStream::GetReadPacket`: the last complete packet. Reads only.
unsafe extern "C" fn get_read_packet(
    this_: *mut c_void,
    packet_number: *mut ULONG,
    flags: *mut DWORD,
    performance_counter: *mut ULONG64,
    more_data: *mut BOOL,
) -> NTSTATUS {
    if packet_number.is_null()
        || flags.is_null()
        || performance_counter.is_null()
        || more_data.is_null()
    {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: called through the input stream vtable, at `INPUT_OFFSET`.
    let stream = unsafe { com::object::<Stream, INPUT_OFFSET>(this_) };
    let per_packet = stream.frames_per_packet.load(Ordering::Relaxed);
    if per_packet == 0 {
        return STATUS_NOT_SUPPORTED;
    }
    if stream.state.load(Ordering::Acquire) < KSSTATE::KSSTATE_PAUSE {
        return STATUS_INVALID_DEVICE_STATE;
    }
    let Some(available) = packet::last_complete(stream.position(), per_packet) else {
        return STATUS_DEVICE_NOT_READY;
    };
    if available == stream.last_read_packet.load(Ordering::Relaxed) {
        return STATUS_DEVICE_NOT_READY;
    }
    stream.last_read_packet.store(available, Ordering::Relaxed);
    // The sampling instant of the packet's first frame.
    let start = stream.start_qpc.load(Ordering::Acquire);
    let offset = clock::ticks_for_frames(
        available.saturating_mul(per_packet),
        stream.qpc_frequency.load(Ordering::Relaxed),
        SAMPLE_RATE,
    );
    #[allow(
        clippy::cast_possible_truncation,
        reason = "packet numbers wrap at 32 bits, as the interface's ULONG does"
    )]
    let number = available as ULONG;
    // SAFETY: the out-parameters are writable (checked not null above).
    unsafe { packet_number.write(number) };
    // SAFETY: as above.
    unsafe { flags.write(0) };
    // SAFETY: as above.
    unsafe { performance_counter.write(start.saturating_add(offset)) };
    // SAFETY: as above. The driver keeps no data beyond the buffer.
    unsafe { more_data.write(FALSE.cast_signed()) };
    STATUS_SUCCESS
}

/// Calls into `IPortWaveRTStream`.
mod port_stream_call {
    use core::ffi::c_void;

    use hostmic_sys::IPortWaveRTStream;
    use wdk_sys::{_MEMORY_CACHING_TYPE, PHYSICAL_ADDRESS, PMDL, PVOID};

    /// The vtable of a live port stream.
    ///
    /// # Safety
    ///
    /// `port_stream` is a live port stream.
    unsafe fn vtable<'a>(
        port_stream: *mut IPortWaveRTStream,
    ) -> &'a hostmic_sys::IPortWaveRTStreamVtbl {
        // SAFETY: the caller's contract.
        let vtable = unsafe { (*port_stream).lpVtbl };
        // SAFETY: an interface's vtable is valid for the interface's lifetime.
        unsafe { &*vtable }
    }

    /// `AllocatePagesForMdl`.
    ///
    /// # Safety
    ///
    /// `port_stream` is a live port stream; `PASSIVE_LEVEL`.
    pub unsafe fn allocate_pages(
        port_stream: *mut IPortWaveRTStream,
        high: PHYSICAL_ADDRESS,
        bytes: usize,
    ) -> Option<PMDL> {
        // SAFETY: the caller's contract.
        let allocate = unsafe { vtable(port_stream) }.AllocatePagesForMdl?;
        // SAFETY: valid port stream and arguments.
        let mdl = unsafe { allocate(port_stream.cast::<c_void>(), high, bytes as u64) };
        (!mdl.is_null()).then_some(mdl)
    }

    /// `MapAllocatedPages`, cached.
    ///
    /// # Safety
    ///
    /// `port_stream` is a live port stream and `mdl` one of its allocations; `PASSIVE_LEVEL`.
    pub unsafe fn map(port_stream: *mut IPortWaveRTStream, mdl: PMDL) -> PVOID {
        // SAFETY: the caller's contract.
        let Some(map) = unsafe { vtable(port_stream) }.MapAllocatedPages else {
            return core::ptr::null_mut();
        };
        // SAFETY: valid port stream and MDL.
        unsafe {
            map(
                port_stream.cast::<c_void>(),
                mdl,
                _MEMORY_CACHING_TYPE::MmCached,
            )
        }
    }

    /// `UnmapAllocatedPages`.
    ///
    /// # Safety
    ///
    /// `base` is the mapping of `mdl` by this port stream; `PASSIVE_LEVEL`.
    pub unsafe fn unmap(port_stream: *mut IPortWaveRTStream, base: PVOID, mdl: PMDL) {
        // SAFETY: the caller's contract.
        if let Some(unmap) = unsafe { vtable(port_stream) }.UnmapAllocatedPages {
            // SAFETY: valid port stream, mapping and MDL.
            unsafe { unmap(port_stream.cast::<c_void>(), base, mdl) };
        }
    }

    /// `FreePagesFromMdl`.
    ///
    /// # Safety
    ///
    /// `mdl` is an unmapped allocation of this port stream; `PASSIVE_LEVEL`.
    pub unsafe fn free_pages(port_stream: *mut IPortWaveRTStream, mdl: PMDL) {
        // SAFETY: the caller's contract.
        if let Some(free) = unsafe { vtable(port_stream) }.FreePagesFromMdl {
            // SAFETY: valid port stream and MDL.
            unsafe { free(port_stream.cast::<c_void>(), mdl) };
        }
    }
}
