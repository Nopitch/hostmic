//! The capture stream: the buffer the audio engine reads and its position, which the host
//! advances (PLAN.md, M4).
//!
//! Ported from `CMiniportWaveRTStream` of Microsoft's `simpleaudiosample`, without its timer:
//! the driver produces no audio and keeps no clock. The host writes frames into the shared
//! buffer and moves the stream position with `ADVANCE` ([`crate::host_link`]), which also
//! signals the audio engine's notification event when a packet completes.
//!
//! - `GetPosition` and `GetReadPacket` only read the position (an atomic) and the packet the
//!   engine read last: no lock, no allocation, any IRQL up to `DISPATCH_LEVEL`.
//! - State changes, buffer allocation and event registration run at `PASSIVE_LEVEL`,
//!   serialized by PortCls, and publish what changes to the host link.
//!
//! The stream's buffer is the start of the adapter's shared buffer ([`crate::shared_buffer`]):
//! `AllocateAudioBuffer` returns a partial MDL over it, sized up from the request
//! (`hostmic_driver_core::buffer::geometry`), and holds a reference to the shared buffer until
//! `FreeAudioBuffer`.

use core::{
    ffi::c_void,
    ptr::{self, NonNull},
    sync::atomic::{AtomicI32, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

use hostmic_abi::{Format, SampleType, State};
use hostmic_driver_core::{
    buffer,
    format::{CHANNELS, FRAME_BYTES, SAMPLE_RATE},
    host::MAX_BUFFER_BYTES,
    packet,
};
use hostmic_sys::{
    BOOL, IMiniportWaveRTInputStreamVtbl, IMiniportWaveRTStreamNotificationVtbl, KSAUDIO_POSITION,
    KSRTAUDIO_HWLATENCY, KSRTAUDIO_HWREGISTER, KSSTATE, PKSAUDIO_POSITION, PKSDATAFORMAT, guid,
};
use wdk_sys::{
    _MEMORY_CACHING_TYPE, DWORD, FALSE, MDL, MEMORY_CACHING_TYPE, NTSTATUS, PKEVENT, PMDL,
    STATUS_DEVICE_NOT_READY, STATUS_INSUFFICIENT_RESOURCES, STATUS_INVALID_DEVICE_STATE,
    STATUS_INVALID_PARAMETER, STATUS_NOT_FOUND, STATUS_NOT_IMPLEMENTED, STATUS_NOT_SUPPORTED,
    STATUS_SUCCESS, STATUS_UNSUCCESSFUL, TRUE, ULONG, ULONG64,
};

use crate::com::{self, ComObject, Unknown};
use crate::host_link::LinkRef;
use crate::shared_buffer::BufferRef;

/// No packet read yet.
const NO_PACKET: u64 = u64::MAX;

/// [`FRAME_BYTES`] as the `u32` the buffer geometry takes.
#[allow(clippy::cast_possible_truncation, reason = "a frame is a few bytes")]
const FRAME_BYTES_U32: u32 = FRAME_BYTES as u32;

/// The capture stream object.
#[repr(C)]
pub struct Stream {
    /// `IUnknown`, `IMiniportWaveRTStream` and `IMiniportWaveRTStreamNotification`, which
    /// extend one another.
    notification: &'static IMiniportWaveRTStreamNotificationVtbl,
    /// `IMiniportWaveRTInputStream` (`GetReadPacket`).
    input: &'static IMiniportWaveRTInputStreamVtbl,
    references: AtomicU32,
    /// `KSSTATE`.
    state: AtomicI32,
    /// The last packet `GetReadPacket` returned, or [`NO_PACKET`]. Only `GetReadPacket` (which
    /// the audio engine calls from one thread) and the stop transition change it.
    last_read_packet: AtomicU64,
    /// The buffer, mapped in system space (non-null while the stream has a buffer), and its
    /// size in bytes (whole frames).
    buffer: AtomicPtr<u8>,
    buffer_bytes: AtomicUsize,
    /// The stream's reference to the shared buffer (a raw [`BufferRef`]) and its MDL over it,
    /// while the stream has a buffer.
    shared: AtomicPtr<c_void>,
    mdl: AtomicPtr<MDL>,
    /// Frames per packet in event-driven mode, 0 in polling mode.
    frames_per_packet: AtomicU64,
    /// The notification event this stream registered with the host link, to release it if
    /// PortCls does not.
    event: AtomicPtr<wdk_sys::_KEVENT>,
    /// The adapter's host link: the position, the host, the engine's event.
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
        // An event or a buffer PortCls did not give back (it always does, before the last
        // release).
        let event = self.event.swap(ptr::null_mut(), Ordering::AcqRel);
        if !event.is_null() {
            self.link.get().clear_engine_event(event);
        }
        self.release_buffer();
        self.link.get().stream_closed();
    }
}

/// Creates a capture stream, or `None` when the pool is exhausted.
#[must_use]
pub fn new(link: LinkRef) -> Option<Unknown> {
    let stream = com::into_unknown(Stream {
        notification: &NOTIFICATION_VTABLE,
        input: &INPUT_VTABLE,
        references: AtomicU32::new(1),
        state: AtomicI32::new(KSSTATE::KSSTATE_STOP),
        last_read_packet: AtomicU64::new(NO_PACKET),
        buffer: AtomicPtr::new(ptr::null_mut()),
        buffer_bytes: AtomicUsize::new(0),
        shared: AtomicPtr::new(ptr::null_mut()),
        mdl: AtomicPtr::new(ptr::null_mut()),
        frames_per_packet: AtomicU64::new(0),
        event: AtomicPtr::new(ptr::null_mut()),
        link,
    })?;
    link.get().stream_opened(Format {
        rate: SAMPLE_RATE,
        channels: CHANNELS,
        sample_type: SampleType::Int16.code(),
    });
    Some(stream)
}

impl Stream {
    /// Frames in the buffer, 0 without one.
    fn buffer_frames(&self) -> u64 {
        (self.buffer_bytes.load(Ordering::Acquire) / FRAME_BYTES) as u64
    }

    /// Takes a buffer in the shared buffer; `packets` is 0 in polling mode.
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
        if !self.buffer.load(Ordering::Acquire).is_null() {
            return STATUS_UNSUCCESSFUL;
        }
        // At least the requested size (the audio engine refuses less), in whole packets of
        // whole frames, within the shared buffer: an exclusive-mode request beyond one second
        // fails here.
        let Some(geometry) =
            buffer::geometry(requested, packets, FRAME_BYTES_U32, MAX_BUFFER_BYTES)
        else {
            wdk::println!("hostmic: {requested} bytes in {packets} packets do not fit the buffer");
            return STATUS_INSUFFICIENT_RESOURCES;
        };
        let Some(shared) = self.link.get().buffer() else {
            return STATUS_DEVICE_NOT_READY;
        };
        let Some(mdl) = shared.stream_mdl(geometry.bytes) else {
            return STATUS_INSUFFICIENT_RESOURCES;
        };
        // Not shared with the audio engine yet; the host writes only once the stream runs.
        shared.zero(geometry.bytes as usize);
        let base = shared.system();

        self.frames_per_packet
            .store(u64::from(geometry.frames_per_packet), Ordering::Relaxed);
        self.buffer_bytes
            .store(geometry.bytes as usize, Ordering::Release);
        self.mdl.store(mdl.as_ptr(), Ordering::Relaxed);
        self.shared.store(shared.into_raw(), Ordering::Release);
        self.buffer.store(base, Ordering::Release);
        self.link
            .get()
            .set_buffer(geometry.frames, geometry.frames_per_packet);
        // SAFETY: the out-parameters are writable (checked not null above).
        unsafe { mdl_out.write(mdl.as_ptr()) };
        // SAFETY: as above.
        unsafe { actual_out.write(geometry.bytes) };
        // SAFETY: as above.
        unsafe { offset_out.write(0) };
        // SAFETY: as above.
        unsafe { cache_out.write(_MEMORY_CACHING_TYPE::MmCached) };
        STATUS_SUCCESS
    }

    /// `FreeAudioBuffer`: PortCls gives back `mdl`, the one `allocate` returned, no longer
    /// mapped into the audio engine. `PASSIVE_LEVEL`, stream stopped.
    fn free(&self, mdl: PMDL) {
        if mdl != self.mdl.load(Ordering::Acquire) {
            wdk::println!("hostmic: FreeAudioBuffer with an MDL this stream did not allocate");
        }
        self.release_buffer();
    }

    /// Lets the buffer go: the host link first (no host advances into it any more), then the
    /// MDL and the reference to the shared buffer. `PASSIVE_LEVEL`.
    fn release_buffer(&self) {
        let base = self.buffer.swap(ptr::null_mut(), Ordering::AcqRel);
        if base.is_null() {
            return;
        }
        self.link.get().set_buffer(0, 0);
        self.buffer_bytes.store(0, Ordering::Release);
        self.frames_per_packet.store(0, Ordering::Relaxed);
        let mdl = self.mdl.swap(ptr::null_mut(), Ordering::AcqRel);
        // SAFETY: the reference `allocate` stored, taken out once.
        let shared =
            unsafe { BufferRef::from_raw(self.shared.swap(ptr::null_mut(), Ordering::AcqRel)) };
        if let (Some(shared), Some(mdl)) = (&shared, NonNull::new(mdl)) {
            // SAFETY: the MDL `allocate` built on this buffer; PortCls no longer maps it (it
            // gave it back, or released the stream).
            unsafe { shared.free_stream_mdl(mdl) };
        }
        drop(shared);
    }

    /// `SetState`. `PASSIVE_LEVEL`, serialized by PortCls.
    fn set_state(&self, state: KSSTATE::Type) -> NTSTATUS {
        wdk::println!("hostmic: SetState {state}");
        let link = self.link.get();
        let previous = self.state.load(Ordering::Acquire);
        match state {
            KSSTATE::KSSTATE_RUN => {
                self.state.store(state, Ordering::Release);
                if previous != KSSTATE::KSSTATE_RUN {
                    link.set_state(State::Running);
                }
            }
            KSSTATE::KSSTATE_PAUSE | KSSTATE::KSSTATE_ACQUIRE => {
                self.state.store(state, Ordering::Release);
                if previous == KSSTATE::KSSTATE_RUN || previous == KSSTATE::KSSTATE_STOP {
                    link.set_state(State::Paused);
                }
            }
            KSSTATE::KSSTATE_STOP => {
                self.state.store(state, Ordering::Release);
                // A stopped stream starts again from position 0 (`Cursor::stopped`).
                self.last_read_packet.store(NO_PACKET, Ordering::Relaxed);
                if previous != KSSTATE::KSSTATE_STOP {
                    link.set_state(State::Stopped);
                }
            }
            _ => return STATUS_INVALID_PARAMETER,
        }
        STATUS_SUCCESS
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
    let frames_in_buffer = stream.buffer_frames();
    let offset = if frames_in_buffer == 0 {
        0
    } else {
        // Below the buffer's frame count, so the product fits the buffer's size.
        (stream.link.get().position() % frames_in_buffer) * FRAME_BYTES as u64
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

/// `RegisterNotificationEvent`: one event, the audio engine's, which the host's `ADVANCE`s
/// signal through the host link.
unsafe extern "C" fn register_notification_event(this_: *mut c_void, event: PKEVENT) -> NTSTATUS {
    if event.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: called through the stream's primary vtable.
    let stream = unsafe { this(this_) };
    if stream
        .event
        .compare_exchange(ptr::null_mut(), event, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return STATUS_UNSUCCESSFUL;
    }
    if !stream.link.get().set_engine_event(event) {
        stream.event.store(ptr::null_mut(), Ordering::Release);
        return STATUS_UNSUCCESSFUL;
    }
    STATUS_SUCCESS
}

/// `UnregisterNotificationEvent`. `PASSIVE_LEVEL`: returns once no `ADVANCE` signals the
/// event any more, since the caller may free it.
unsafe extern "C" fn unregister_notification_event(this_: *mut c_void, event: PKEVENT) -> NTSTATUS {
    // SAFETY: called through the stream's primary vtable.
    let stream = unsafe { this(this_) };
    if stream
        .event
        .compare_exchange(event, ptr::null_mut(), Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return STATUS_NOT_FOUND;
    }
    stream.link.get().clear_engine_event(event);
    STATUS_SUCCESS
}

/// `IMiniportWaveRTInputStream::GetReadPacket`: the oldest complete packet the engine has not
/// read, with the performance counter value of its first frame. Any IRQL up to
/// `DISPATCH_LEVEL`; no lock.
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
    let link = stream.link.get();
    let last = stream.last_read_packet.load(Ordering::Relaxed);
    let Some(read) = packet::next_read(
        link.position(),
        per_packet,
        stream.buffer_frames(),
        (last != NO_PACKET).then_some(last),
    ) else {
        return STATUS_DEVICE_NOT_READY;
    };
    stream
        .last_read_packet
        .store(read.packet, Ordering::Relaxed);
    if read.skipped != 0 {
        link.packets_skipped(read.skipped);
    }
    let counter = link.frame_qpc(read.packet.saturating_mul(per_packet));
    #[allow(
        clippy::cast_possible_truncation,
        reason = "packet numbers wrap at 32 bits, as the interface's ULONG does"
    )]
    let number = read.packet as ULONG;
    // SAFETY: the out-parameters are writable (checked not null above).
    unsafe { packet_number.write(number) };
    // SAFETY: as above.
    unsafe { flags.write(0) };
    // SAFETY: as above.
    unsafe { performance_counter.write(counter) };
    // SAFETY: as above.
    unsafe { more_data.write(if read.more { TRUE } else { FALSE }.cast_signed()) };
    STATUS_SUCCESS
}
