//! The reference loop: a real-time thread that feeds the microphone from a callback.

use core::{
    ffi::c_void,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    time::Duration,
};
use std::{
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    sync::Arc,
    thread::{self, JoinHandle},
};

use hostmic_abi::{
    Format, FormatRange, GUID_DEVINTERFACE_HOSTMIC_HOST, SampleType, State, StreamState,
};
use windows::{
    Win32::{
        Devices::DeviceAndDriverInstallation::{
            CM_NOTIFY_ACTION, CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL,
            CM_NOTIFY_ACTION_DEVICEQUERYREMOVE, CM_NOTIFY_ACTION_DEVICEQUERYREMOVEFAILED,
            CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE, CM_NOTIFY_ACTION_DEVICEREMOVEPENDING,
            CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER, CM_NOTIFY_FILTER_0, CM_NOTIFY_FILTER_0_0,
            CM_NOTIFY_FILTER_0_1, CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE,
            CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE, CM_Register_Notification,
            CM_Unregister_Notification, CR_SUCCESS, HCMNOTIFICATION, PCM_NOTIFY_CALLBACK,
        },
        Foundation::{ERROR_SUCCESS, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::{
            Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
            Threading::{
                AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CancelWaitableTimer, CreateEventW,
                CreateWaitableTimerExW, ResetEvent, SetEvent, SetWaitableTimer, TIMER_ALL_ACCESS,
                WaitForMultipleObjects, WaitForSingleObject,
            },
        },
    },
    core::{GUID, HRESULT, PCWSTR, w},
};

use crate::{
    Advance, Error, Host,
    clock::{self, Anchor, Due},
    convert, interfaces, stream,
};

/// The frames a [`Player`]'s callback fills: interleaved `f32` samples, nominal range -1.0 to
/// 1.0, zeroed before the call. The player converts them to the stream's sample type.
///
/// Blocks vary in size: normally about a millisecond of frames (47, 48, 49… at 48 kHz),
/// more when the thread woke up late, and a whole stream buffer when it was more than a buffer
/// late. A callback with fixed-size processing buffers internally. Frames the driver then
/// refuses (the stream changed meanwhile) are not delivered: the next block starts again from
/// the stream's position.
#[derive(Debug)]
pub struct Block<'a> {
    format: Format,
    position: u64,
    due: i64,
    frequency: i64,
    channels: usize,
    samples: &'a mut [f32],
}

impl Block<'_> {
    /// The stream's format: its rate, channels and the sample type the block is converted to.
    #[must_use]
    pub fn format(&self) -> Format {
        self.format
    }

    /// The stream position of the block's first frame: frames since the stream started.
    #[must_use]
    pub fn position(&self) -> u64 {
        self.position
    }

    /// When the block's first frame is due by the player's clock: a performance counter value
    /// (`QueryPerformanceCounter`), comparable with another clock's, such as a physical
    /// microphone's, to measure drift or latency.
    #[must_use]
    pub fn due(&self) -> i64 {
        self.due
    }

    /// Performance counter ticks per second, for [`due`](Self::due).
    #[must_use]
    pub fn frequency(&self) -> i64 {
        self.frequency
    }

    /// Frames in the block.
    #[must_use]
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1)
    }

    /// Samples per frame.
    #[must_use]
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The samples to fill, frame after frame.
    pub fn samples_mut(&mut self) -> &mut [f32] {
        self.samples
    }
}

/// What a [`Player`] has done so far. Each counter is read on its own: they agree with one
/// another only to within a period.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlayerStats {
    /// Frames written and advanced.
    pub frames_written: u64,
    /// `ADVANCE` requests accepted.
    pub advances: u64,
    /// Writes refused because the stream had changed meanwhile ([`Advance::Stale`]); each one
    /// makes the player read the stream again.
    pub stale: u64,
    /// Times the player was more than a stream buffer late (frames lost, clock anchored again).
    pub late: u64,
    /// The longest time between two wake-ups that wrote while the stream ran: how late the
    /// player ever was.
    pub longest_gap: Duration,
    /// The stream's state as the player last read it; `None` before the first read and while
    /// detached.
    pub state: Option<State>,
    /// Whether the player is attached to a microphone now.
    pub attached: bool,
    /// Attachments so far: 1 for [`Player::start`]; with [`Player::follow`], one more each time
    /// the microphone comes back.
    pub attachments: u64,
    /// Times a following player let the microphone go: Windows asked to remove it, or the
    /// driver detached it ([`Player::follow`] only).
    pub releases: u64,
    /// Whether the thread runs under MMCSS ("Pro Audio").
    pub mmcss: bool,
    /// Whether the thread has ended: stopped, on an error that [`Player::stop`] returns, or on
    /// a panic of the callback, which it resumes.
    pub finished: bool,
}

/// What the player's thread, its owner and its notification callbacks share.
struct Shared {
    /// Signaled by [`Player::stop`]; manual reset.
    stop: OwnedHandle,
    /// Signaled when the microphone's interface may have arrived, or a removal was cancelled:
    /// the detached thread tries to attach. Auto-reset.
    arrival: OwnedHandle,
    /// Signaled when Windows asks to remove the microphone: the thread detaches and closes its
    /// handle. Auto-reset; only counts while `pending` or `gone` is set, so that a stale signal
    /// never releases a later attachment.
    release: OwnedHandle,
    /// Signaled by the thread once released, for the callback that waits for it. Manual
    /// reset, reset by that callback before it asks.
    released: OwnedHandle,
    /// Set by the callback for a removal request, cleared by the release that answers it.
    pending: AtomicBool,
    /// Set by the callback when Windows asks to remove the device, until the removal is
    /// cancelled or complete: the thread must not attach meanwhile,
    /// or its new handle would make the driver refuse the removal it just made way for.
    removing: AtomicBool,
    /// Set by the callback when the device is gone (removal pending or complete): the thread
    /// drops its registration, which belonged to that device.
    gone: AtomicBool,
    frequency: i64,
    frames_written: AtomicU64,
    advances: AtomicU64,
    stale: AtomicU64,
    late: AtomicU64,
    longest_gap_ticks: AtomicU64,
    /// A [`State`] code, or `u32::MAX` before the first read and while detached.
    state: AtomicU32,
    attached: AtomicBool,
    attachments: AtomicU64,
    releases: AtomicU64,
    mmcss: AtomicBool,
    finished: AtomicBool,
    /// Set by the thread as it ends, before it answers a waiting request one last time: a
    /// request that comes later returns at once instead of waiting for a thread that is gone.
    closing: AtomicBool,
}

impl core::fmt::Debug for Shared {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
}

/// An unnamed event.
fn event(manual_reset: bool) -> Result<OwnedHandle, Error> {
    // SAFETY: no security attributes; an unnamed event, not signaled.
    let event =
        unsafe { CreateEventW(None, manual_reset, false, PCWSTR::null()) }.map_err(|error| {
            Error::Os {
                call: "CreateEventW",
                error,
            }
        })?;
    // SAFETY: a new handle that nothing else owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(event.0) })
}

/// Signals `event`. Cannot fail for a valid event handle.
fn signal(event: &OwnedHandle) {
    // SAFETY: an event handle its owner keeps open.
    let _ = unsafe { SetEvent(HANDLE(event.as_raw_handle())) };
}

/// The reference host loop: a thread that owns a [`Host`] and, while the stream runs, writes
/// what a callback produces, at the pace of the performance counter.
///
/// The thread registers with MMCSS ("Pro Audio"). While the stream runs, it wakes up about
/// every millisecond on a high-resolution waitable timer (Windows 10 1803 and later),
/// computes the frames due since the stream started running, has the callback fill them in
/// one block, and writes them in chunks of about a millisecond, each copied just before its
/// own `ADVANCE`, as the host interface asks. While the stream does not run, the timer is off
/// and only the stream's event and the stop event wake the thread.
///
/// Two ways to start it: [`start`](Self::start) with a [`Host`] attached by the caller, for as
/// long as that attachment lasts; [`follow`](Self::follow), which attaches by itself, lets the
/// microphone go when Windows asks to remove it (a driver update, the device disabled), and
/// attaches again when it comes back. A removal still waits for a reboot when another handle
/// to the microphone stays open: a [`Device`](crate::Device) kept open to read the driver's
/// counters, for one, or a request that came between the player's open and its registration
/// for removal requests (a window of a few microseconds per attachment).
///
/// It allocates nothing, takes no lock and performs no I/O besides its requests to the driver;
/// the callback must keep that rule too (PLAN.md, "Real-time rule"). It reports through
/// [`stats`](Self::stats), never by printing.
#[derive(Debug)]
pub struct Player {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<Result<Option<Host>, Error>>>,
}

impl Player {
    /// Starts the loop on its own thread, which takes `host`; `callback` fills each block. The
    /// loop ends with an error when the attachment does ([`Error::NotHost`]).
    ///
    /// # Errors
    ///
    /// [`Error::Os`] when an event, the high-resolution timer or the thread cannot be created;
    /// `host` is then dropped, which detaches it.
    pub fn start<F>(host: Host, callback: F) -> Result<Self, Error>
    where
        F: FnMut(&mut Block<'_>) + Send + 'static,
    {
        Self::spawn(Some(host), None, callback)
    }

    /// Starts the loop on its own thread, which follows the first hostmic microphone: it
    /// attaches with `formats` as soon as the microphone's host interface is there (now or
    /// later), plays with `callback` while attached, detaches and closes its handle when
    /// Windows asks to remove the microphone, so that the removal goes through, and attaches
    /// again when the microphone comes back or the removal is cancelled. Another host attached
    /// meanwhile is retried every second.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRange`] for an empty, inverted or unknown range; [`Error::Os`] when an
    /// event, the timer or the thread cannot be created. Errors met later end the loop, and
    /// [`stop`](Self::stop) returns them: another ABI version, no common format, a registration
    /// for notifications refused by the configuration manager.
    pub fn follow<F>(formats: FormatRange, callback: F) -> Result<Self, Error>
    where
        F: FnMut(&mut Block<'_>) + Send + 'static,
    {
        if !formats.is_valid() {
            return Err(Error::InvalidRange(formats));
        }
        Self::spawn(None, Some(formats), callback)
    }

    fn spawn<F>(host: Option<Host>, follow: Option<FormatRange>, callback: F) -> Result<Self, Error>
    where
        F: FnMut(&mut Block<'_>) + Send + 'static,
    {
        let mut frequency = 0;
        // SAFETY: a writable frequency.
        unsafe { QueryPerformanceFrequency(&raw mut frequency) }.map_err(|error| Error::Os {
            call: "QueryPerformanceFrequency",
            error,
        })?;
        // SAFETY: no security attributes, no name; documented flags and access.
        let timer = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        }
        .map_err(|error| Error::Os {
            call: "CreateWaitableTimerExW (high resolution)",
            error,
        })?;
        // SAFETY: a new handle that nothing else owns.
        let timer = unsafe { OwnedHandle::from_raw_handle(timer.0) };
        let attached = host.is_some();
        let shared = Arc::new(Shared {
            stop: event(true)?,
            arrival: event(false)?,
            release: event(false)?,
            released: event(true)?,
            pending: AtomicBool::new(false),
            removing: AtomicBool::new(false),
            gone: AtomicBool::new(false),
            frequency,
            frames_written: AtomicU64::new(0),
            advances: AtomicU64::new(0),
            stale: AtomicU64::new(0),
            late: AtomicU64::new(0),
            longest_gap_ticks: AtomicU64::new(0),
            state: AtomicU32::new(u32::MAX),
            attached: AtomicBool::new(attached),
            attachments: AtomicU64::new(u64::from(attached)),
            releases: AtomicU64::new(0),
            mmcss: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            closing: AtomicBool::new(false),
        });
        // The largest block: the whole mapping in the narrowest sample type. A followed
        // microphone's buffers are sized when the player attaches.
        let buffer_bytes = host.as_ref().map_or(0, Host::buffer_bytes);
        let samples = vec![0.0f32; buffer_bytes / 2];
        let bytes = vec![0u8; buffer_bytes];
        let thread_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("hostmic player".to_owned())
            .spawn(move || {
                // Marks the end however the thread ends, a panic of the callback included.
                let _finished = Finished(Arc::clone(&thread_shared));
                let mut work = Work {
                    handle_notification: None,
                    interface_notification: None,
                    host,
                    follow,
                    timer,
                    armed: false,
                    shared: thread_shared,
                    samples,
                    bytes,
                };
                let result = work.run(callback);
                // A removal request waiting for this thread is answered, its handle closed
                // first (on an error too), before the registrations go: unregistering waits
                // for the callback. Later requests do not wait (`closing`).
                if work.shared.pending.load(Ordering::Acquire) {
                    work.release();
                }
                work.shared.closing.store(true, Ordering::Release);
                signal(&work.shared.released);
                result.map(|()| work.host.take())
            })
            .map_err(|error| Error::Os {
                call: "CreateThread",
                error: windows::core::Error::from_hresult(HRESULT::from_win32(
                    error
                        .raw_os_error()
                        .and_then(|code| u32::try_from(code).ok())
                        .unwrap_or(0x1F), // ERROR_GEN_FAILURE
                )),
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// What the loop has done so far; from any thread.
    #[must_use]
    pub fn stats(&self) -> PlayerStats {
        let shared = &self.shared;
        let ticks = shared.longest_gap_ticks.load(Ordering::Relaxed);
        let frequency = u64::try_from(shared.frequency).unwrap_or(1).max(1);
        let nanos = u128::from(ticks) * 1_000_000_000 / u128::from(frequency);
        PlayerStats {
            frames_written: shared.frames_written.load(Ordering::Relaxed),
            advances: shared.advances.load(Ordering::Relaxed),
            stale: shared.stale.load(Ordering::Relaxed),
            late: shared.late.load(Ordering::Relaxed),
            longest_gap: Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX)),
            state: State::from_code(shared.state.load(Ordering::Relaxed)),
            attached: shared.attached.load(Ordering::Relaxed),
            attachments: shared.attachments.load(Ordering::Relaxed),
            releases: shared.releases.load(Ordering::Relaxed),
            mmcss: shared.mmcss.load(Ordering::Relaxed),
            finished: shared.finished.load(Ordering::Acquire),
        }
    }

    /// Stops the loop and hands the [`Host`] back, still attached; `None` when the player was
    /// following a microphone and is detached now. A following player's `Host` comes back
    /// without its registration for removal requests: kept open, it makes a removal wait for
    /// a reboot; detach it when done.
    ///
    /// # Errors
    ///
    /// The error that ended the loop before, if any ([`Error::NotHost`] when the driver
    /// detached a host given to [`start`](Self::start), for one); the `Host` is then dropped.
    ///
    /// # Panics
    ///
    /// Resumes the callback's panic, if it panicked.
    pub fn stop(mut self) -> Result<Option<Host>, Error> {
        signal(&self.shared.stop);
        let thread = self.thread.take().expect("joined only here or in drop");
        match thread.join() {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }
}

impl Drop for Player {
    /// Stops the loop; the host is dropped with it, which detaches it.
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            signal(&self.shared.stop);
            let _ = thread.join();
        }
    }
}

/// Sets [`PlayerStats::finished`] when dropped.
struct Finished(Arc<Shared>);

impl Drop for Finished {
    fn drop(&mut self) {
        self.0.finished.store(true, Ordering::Release);
    }
}

/// Undoes the thread's MMCSS registration and stops its timer when dropped, a panic included.
struct Registration {
    mmcss: Option<HANDLE>,
    timer: HANDLE,
}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: the player's timer, open while `Work` lives, which outlives this guard.
        let _ = unsafe { CancelWaitableTimer(self.timer) };
        if let Some(mmcss) = self.mmcss {
            // SAFETY: the handle `AvSetMmThreadCharacteristicsW` returned on this thread; the
            // guard never leaves it.
            let _ = unsafe { AvRevertMmThreadCharacteristics(mmcss) };
        }
    }
}

/// The performance counter.
fn qpc() -> i64 {
    let mut now = 0;
    // SAFETY: a writable counter; cannot fail on Windows XP and later.
    let _ = unsafe { QueryPerformanceCounter(&raw mut now) };
    now
}

/// A configuration manager notification registration, unregistered when dropped. Dropped on
/// the player's thread only, never in a callback: unregistering waits for the callbacks in
/// flight.
struct Notification(HCMNOTIFICATION);

impl Notification {
    /// Registers `callback` for `filter`, with the player's shared state as its context.
    fn register(
        filter: &CM_NOTIFY_FILTER,
        shared: &Arc<Shared>,
        callback: PCM_NOTIFY_CALLBACK,
    ) -> Result<Self, Error> {
        let mut handle = HCMNOTIFICATION::default();
        // SAFETY: a filter of the documented size; the context is the shared state, which
        // outlives the registration (`Work` drops its notifications before its `Arc`); a
        // writable registration handle.
        let status = unsafe {
            CM_Register_Notification(
                filter,
                Some(Arc::as_ptr(shared).cast::<c_void>()),
                callback,
                &raw mut handle,
            )
        };
        if status == CR_SUCCESS {
            Ok(Self(handle))
        } else {
            Err(Error::Os {
                call: "CM_Register_Notification",
                error: windows::core::Error::new(
                    HRESULT(0x8000_4005_u32.cast_signed()),
                    format!("CONFIGRET {:#x}", status.0),
                ),
            })
        }
    }

    /// Arrivals of hostmic host interfaces.
    fn interfaces(shared: &Arc<Shared>) -> Result<Self, Error> {
        let filter = CM_NOTIFY_FILTER {
            cbSize: u32::try_from(size_of::<CM_NOTIFY_FILTER>()).unwrap_or(0),
            FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
            u: CM_NOTIFY_FILTER_0 {
                DeviceInterface: CM_NOTIFY_FILTER_0_0 {
                    ClassGuid: GUID::from_values(
                        GUID_DEVINTERFACE_HOSTMIC_HOST.data1,
                        GUID_DEVINTERFACE_HOSTMIC_HOST.data2,
                        GUID_DEVINTERFACE_HOSTMIC_HOST.data3,
                        GUID_DEVINTERFACE_HOSTMIC_HOST.data4,
                    ),
                },
            },
            ..Default::default()
        };
        Self::register(&filter, shared, Some(on_interface))
    }

    /// Removal requests for the device behind `handle`.
    fn handle(handle: HANDLE, shared: &Arc<Shared>) -> Result<Self, Error> {
        let filter = CM_NOTIFY_FILTER {
            cbSize: u32::try_from(size_of::<CM_NOTIFY_FILTER>()).unwrap_or(0),
            FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE,
            u: CM_NOTIFY_FILTER_0 {
                DeviceHandle: CM_NOTIFY_FILTER_0_1 { hTarget: handle },
            },
            ..Default::default()
        };
        Self::register(&filter, shared, Some(on_handle))
    }
}

impl Drop for Notification {
    fn drop(&mut self) {
        // SAFETY: a registration this value owns, unregistered once, outside any callback.
        let _ = unsafe { CM_Unregister_Notification(self.0) };
    }
}

/// The interface notification callback: an arrival may be the followed microphone.
///
/// # Safety
///
/// Called by the configuration manager with the context given at registration: the shared
/// state, alive until the registration is gone.
unsafe extern "system" fn on_interface(
    _notification: HCMNOTIFICATION,
    context: *const c_void,
    action: CM_NOTIFY_ACTION,
    _data: *const CM_NOTIFY_EVENT_DATA,
    _size: u32,
) -> u32 {
    // SAFETY: the shared state given at registration, alive while it is registered.
    let shared = unsafe { &*context.cast::<Shared>() };
    if action == CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL {
        signal(&shared.arrival);
    }
    ERROR_SUCCESS.0
}

/// How long a removal request waits for the player to let the microphone go. Normally a
/// period or two; past it the request returns with the handle still open, the driver refuses
/// the removal, and nothing retries it: the user repeats the removal or reboots.
const RELEASE_WAIT_MS: u32 = 2_000;

/// The device handle notification callback. On a removal request, the player detaches and
/// closes its handle before this returns, as the configuration manager requires: the player's
/// thread writes into the mapping, so it must stop before the handle goes, and this callback
/// waits for it.
///
/// # Safety
///
/// As [`on_interface`].
unsafe extern "system" fn on_handle(
    _notification: HCMNOTIFICATION,
    context: *const c_void,
    action: CM_NOTIFY_ACTION,
    _data: *const CM_NOTIFY_EVENT_DATA,
    _size: u32,
) -> u32 {
    // SAFETY: the shared state given at registration, alive while it is registered.
    let shared = unsafe { &*context.cast::<Shared>() };
    if action == CM_NOTIFY_ACTION_DEVICEQUERYREMOVE {
        // No attaching until the removal is decided, then a release requested.
        shared.removing.store(true, Ordering::Release);
        shared.pending.store(true, Ordering::Release);
        // SAFETY: an event the shared state owns; reset before asking, so that only this
        // request's answer counts.
        let _ = unsafe { ResetEvent(HANDLE(shared.released.as_raw_handle())) };
        // Checked after the reset: if the thread sets `closing` later, its last signal of
        // `released` still wakes this wait.
        if shared.closing.load(Ordering::Acquire) {
            return ERROR_SUCCESS.0;
        }
        signal(&shared.release);
        // SAFETY: an event the shared state owns. A timeout leaves the handle open, which
        // makes the driver refuse the removal (see `RELEASE_WAIT_MS`).
        let _ = unsafe {
            WaitForSingleObject(HANDLE(shared.released.as_raw_handle()), RELEASE_WAIT_MS)
        };
    } else if action == CM_NOTIFY_ACTION_DEVICEQUERYREMOVEFAILED {
        // The device stays: attach again.
        shared.removing.store(false, Ordering::Release);
        signal(&shared.arrival);
    } else if action == CM_NOTIFY_ACTION_DEVICEREMOVEPENDING
        || action == CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE
    {
        // Gone, with or without a request: let go if still attached, and drop this
        // registration, from the player's thread. Attaching may resume: the interface is
        // gone, and a new one announces itself.
        shared.gone.store(true, Ordering::Release);
        shared.removing.store(false, Ordering::Release);
        signal(&shared.release);
    }
    ERROR_SUCCESS.0
}

/// How [`Work::play`] ended.
enum Ended {
    /// The stop event.
    Stopped,
    /// The microphone was let go (a removal request, or the driver detached a followed host).
    Released,
}

/// The player thread's state.
struct Work {
    /// Dropped first, while `shared`, their context, is alive. The handle registration goes
    /// with the interface path it was made for.
    handle_notification: Option<(Notification, String)>,
    interface_notification: Option<Notification>,
    host: Option<Host>,
    /// The formats to attach with, when following the microphone.
    follow: Option<FormatRange>,
    timer: OwnedHandle,
    /// Whether the timer runs: only while the stream accepts frames.
    armed: bool,
    shared: Arc<Shared>,
    /// The callback's block, preallocated for the largest stream.
    samples: Vec<f32>,
    /// The encoded block.
    bytes: Vec<u8>,
}

impl Work {
    /// Registers with MMCSS and serves until the stop event or an error.
    fn run<F: FnMut(&mut Block<'_>)>(&mut self, mut callback: F) -> Result<(), Error> {
        let mut task = 0u32;
        // SAFETY: a static task name and a writable task index (0: a new task). Failure only
        // loses priority.
        let mmcss = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &raw mut task) }.ok();
        self.shared.mmcss.store(mmcss.is_some(), Ordering::Relaxed);
        let _registration = Registration {
            mmcss,
            timer: HANDLE(self.timer.as_raw_handle()),
        };
        if self.follow.is_some() {
            // Registered before listing, as Microsoft advises: an interface arriving in
            // between is seen by both, never by neither.
            self.interface_notification = Some(Notification::interfaces(&self.shared)?);
        }
        loop {
            if self.host.is_some() {
                match self.play(&mut callback) {
                    Ok(Ended::Stopped) => {
                        // A removal request that came with the stop is answered first.
                        if self.shared.pending.load(Ordering::Acquire) {
                            self.release();
                        }
                        return Ok(());
                    }
                    Ok(Ended::Released) => {}
                    Err(Error::NotHost) if self.follow.is_some() => self.release(),
                    Err(error) => return Err(error),
                }
            } else if self.wait_for_microphone()? {
                return Ok(());
            }
        }
    }

    /// Detached, following the microphone: attaches if it can, else waits for an arrival, a
    /// cancelled removal or a second, whichever comes first. While a removal is being decided
    /// it does not attach: a new handle would make the driver refuse that removal. Returns
    /// whether to stop.
    fn wait_for_microphone(&mut self) -> Result<bool, Error> {
        let Some(formats) = self.follow else {
            // A started player without its host: nothing left to do.
            return Err(Error::NotHost);
        };
        let removing = self.shared.removing.load(Ordering::Acquire);
        if !removing && self.try_attach(formats)? {
            return Ok(false);
        }
        let handles = [
            HANDLE(self.shared.stop.as_raw_handle()),
            HANDLE(self.shared.release.as_raw_handle()),
            HANDLE(self.shared.arrival.as_raw_handle()),
        ];
        // While removing, the callbacks end the wait (cancelled or complete);
        // a minute is the safety net should none come.
        let timeout = if removing { 60_000 } else { 1_000 };
        // SAFETY: three handles the shared state keeps open.
        let wait = unsafe { WaitForMultipleObjects(&handles, false, timeout) };
        if wait == WAIT_OBJECT_0 {
            Ok(true)
        } else if wait.0 == WAIT_OBJECT_0.0 + 1 {
            // A request met detached, or the device gone: answer it, drop the registration.
            self.release();
            Ok(false)
        } else if wait == WAIT_FAILED {
            Err(Error::Os {
                call: "WaitForMultipleObjects",
                error: windows::core::Error::from_thread(),
            })
        } else {
            if wait == WAIT_TIMEOUT && removing {
                self.shared.removing.store(false, Ordering::Release);
            }
            Ok(false)
        }
    }

    /// Attaches to the first hostmic microphone with `formats`, if there is one and it is
    /// free; registers for its removal requests unless already registered on that device.
    /// Returns whether it attached.
    fn try_attach(&mut self, formats: FormatRange) -> Result<bool, Error> {
        // The configuration manager's list may fail while devices change: later.
        let Some(path) = interfaces().ok().and_then(|paths| paths.into_iter().next()) else {
            return Ok(false);
        };
        let host = match Host::attach(&path, formats) {
            Ok(host) => host,
            // Another host, a device being removed or restarted: later.
            Err(Error::Busy | Error::Os { .. } | Error::NotHost) => return Ok(false),
            Err(error) => return Err(error),
        };
        // A registration on the device stays valid after its handle closes (Microsoft's
        // "Registering for Notification of Device Interface Arrival and Device Removal"): one
        // per device, kept across reattachments, dropped once the device is gone. One made for
        // another interface (another microphone) is replaced: a callback waiting on it is
        // answered first, since unregistering waits for it, and no handle to its device is
        // open now.
        let same = self
            .handle_notification
            .as_ref()
            .is_some_and(|(_, registered)| *registered == path)
            && !self.shared.gone.load(Ordering::Acquire);
        if !same {
            if self.handle_notification.is_some() {
                signal(&self.shared.released);
                self.handle_notification = None;
            }
            self.shared.gone.store(false, Ordering::Release);
            self.handle_notification = Some((
                Notification::handle(host.file_handle(), &self.shared)?,
                path,
            ));
        }
        // Sized once per attachment, outside the real-time loop.
        let buffer_bytes = host.buffer_bytes();
        if self.bytes.len() < buffer_bytes {
            self.samples.resize(buffer_bytes / 2, 0.0);
            self.bytes.resize(buffer_bytes, 0);
        }
        self.host = Some(host);
        self.shared.attached.store(true, Ordering::Relaxed);
        self.shared.attachments.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }

    /// Lets the microphone go: stops the timer, detaches and closes the handle (if still
    /// attached), then tells a waiting removal request that it may go on.
    fn release(&mut self) {
        self.disarm();
        if let Some(host) = self.host.take() {
            // Refused when the driver already detached it: the handle closes all the same.
            let _ = host.detach();
            self.shared.releases.fetch_add(1, Ordering::Relaxed);
        }
        self.shared.attached.store(false, Ordering::Relaxed);
        self.shared.state.store(u32::MAX, Ordering::Relaxed);
        self.shared.pending.store(false, Ordering::Release);
        signal(&self.shared.released);
        if self.shared.gone.swap(false, Ordering::AcqRel) {
            // From this thread, never in the callback: unregistering waits for callbacks.
            self.handle_notification = None;
        }
    }

    /// Starts the timer: first in 1 ms, then every millisecond.
    fn arm(&mut self) -> Result<(), Error> {
        if !self.armed {
            // Relative, in 100 ns units.
            let due: i64 = -10_000;
            // SAFETY: a timer this thread owns; no completion routine.
            unsafe {
                SetWaitableTimer(
                    HANDLE(self.timer.as_raw_handle()),
                    &raw const due,
                    1,
                    None,
                    None,
                    false,
                )
            }
            .map_err(|error| Error::Os {
                call: "SetWaitableTimer",
                error,
            })?;
            self.armed = true;
        }
        Ok(())
    }

    /// Stops the timer while the stream does not run.
    fn disarm(&mut self) {
        if self.armed {
            // SAFETY: a timer this thread owns.
            let _ = unsafe { CancelWaitableTimer(HANDLE(self.timer.as_raw_handle())) };
            self.armed = false;
        }
    }

    /// Plays while attached, until the stop event, a release or an error.
    fn play<F: FnMut(&mut Block<'_>)>(&mut self, callback: &mut F) -> Result<Ended, Error> {
        let Some(host) = self.host.as_ref() else {
            return Ok(Ended::Released);
        };
        let handles = [
            HANDLE(self.shared.stop.as_raw_handle()),
            HANDLE(self.shared.release.as_raw_handle()),
            HANDLE(host.event().as_raw_handle()),
            HANDLE(self.timer.as_raw_handle()),
        ];
        let frequency = self.shared.frequency;
        let mut stream = StreamState::default();
        let mut anchor: Option<Anchor> = None;
        let mut position = 0u64;
        let mut last_write: Option<i64> = None;
        // Read the stream first, as if its event had been signaled.
        let mut read = true;
        loop {
            if read {
                read = false;
                let previous = stream;
                stream = self.host.as_ref().ok_or(Error::NotHost)?.stream()?;
                self.shared.state.store(stream.state, Ordering::Relaxed);
                if !stream::accepts_frames(&stream) {
                    anchor = None;
                    last_write = None;
                    self.disarm();
                } else if anchor.is_none() || stream.generation != previous.generation {
                    anchor = Some(Anchor {
                        qpc: qpc(),
                        position: stream.position,
                    });
                    position = stream.position;
                    last_write = None;
                    self.arm()?;
                }
            }
            // SAFETY: four handles this thread, its owner or its host keep open for the loop.
            let wait = unsafe { WaitForMultipleObjects(&handles, false, 100) };
            if wait == WAIT_OBJECT_0 {
                return Ok(Ended::Stopped);
            } else if wait.0 == WAIT_OBJECT_0.0 + 1 {
                // A stale signal (a request already answered) is no reason to let go.
                if self.shared.pending.load(Ordering::Acquire)
                    || self.shared.gone.load(Ordering::Acquire)
                {
                    self.release();
                    return Ok(Ended::Released);
                }
                continue;
            } else if wait.0 == WAIT_OBJECT_0.0 + 2 {
                read = true;
                continue;
            } else if wait == WAIT_FAILED {
                return Err(Error::Os {
                    call: "WaitForMultipleObjects",
                    error: windows::core::Error::from_thread(),
                });
            } else if wait != WAIT_TIMEOUT && wait.0 != WAIT_OBJECT_0.0 + 3 {
                continue;
            }
            let Some(start) = anchor else {
                continue;
            };
            let now = qpc();
            let frames = match clock::due(
                start,
                now,
                position,
                stream.format.rate,
                frequency,
                stream.buffer_frames,
            ) {
                Due::Nothing => continue,
                Due::Frames(frames) => frames,
                Due::Late { .. } => {
                    self.shared.late.fetch_add(1, Ordering::Relaxed);
                    anchor = Some(Anchor {
                        qpc: now,
                        position: position.saturating_add(u64::from(stream.buffer_frames)),
                    });
                    stream.buffer_frames
                }
            };
            if let Some(previous) = last_write {
                let gap = u64::try_from(now.saturating_sub(previous)).unwrap_or(0);
                self.shared
                    .longest_gap_ticks
                    .fetch_max(gap, Ordering::Relaxed);
            }
            last_write = Some(now);
            // By the clock the frames were due by (before any re-anchoring for lateness).
            let due = start.qpc.saturating_add(clock::ticks_for(
                position.saturating_sub(start.position),
                stream.format.rate,
                frequency,
            ));
            let (outcome, written) = self.write(&stream, position, due, frames, callback)?;
            position = position.saturating_add(written);
            if outcome == Advance::Stale || outcome == Advance::NotRunning {
                self.shared.stale.fetch_add(1, Ordering::Relaxed);
                anchor = None;
                read = true;
            }
        }
    }

    /// Has the callback fill `frames` frames from `position`, encodes them, and writes them in
    /// chunks of about a millisecond, each just before its `ADVANCE`. Returns the last
    /// outcome and the frames advanced; a refused chunk ends the write.
    fn write<F: FnMut(&mut Block<'_>)>(
        &mut self,
        stream: &StreamState,
        position: u64,
        due: i64,
        frames: u32,
        callback: &mut F,
    ) -> Result<(Advance, u64), Error> {
        let sample_type = SampleType::from_code(stream.format.sample_type)
            .ok_or(Error::Write("unknown stream format"))?;
        // Checked before the callback: a stream without a whole frame size writes nothing.
        let frame_bytes = stream::frame_bytes(&stream.format)
            .ok_or(Error::Write("unknown stream format"))? as usize;
        let channels = usize::from(stream.format.channels);
        let count = (frames as usize)
            .checked_mul(channels)
            .ok_or(Error::Write("block larger than the mapping"))?;
        let samples = self
            .samples
            .get_mut(..count)
            .ok_or(Error::Write("block larger than the mapping"))?;
        samples.fill(0.0);
        callback(&mut Block {
            format: stream.format,
            position,
            due,
            frequency: self.shared.frequency,
            channels,
            samples: &mut *samples,
        });
        convert::encode(samples, sample_type, &mut self.bytes)
            .ok_or(Error::Write("block larger than the mapping"))?;
        // About a millisecond per request, as the host interface asks: parts of near-equal
        // size, between one and two milliseconds each.
        let parts = clock::parts(frames, stream.format.rate.div_ceil(1_000));
        let mut at = position;
        let mut written = 0u64;
        let mut outcome = Advance::Advanced(position);
        for index in 0..parts {
            let first = clock::part_start(frames, parts, index) as usize * frame_bytes;
            let end = clock::part_start(frames, parts, index + 1) as usize * frame_bytes;
            let host = self.host.as_mut().ok_or(Error::NotHost)?;
            outcome = host.write(stream, at, &self.bytes[first..end])?;
            match outcome {
                Advance::Advanced(next) => {
                    let advanced = next.saturating_sub(at);
                    written += advanced;
                    at = next;
                    self.shared.advances.fetch_add(1, Ordering::Relaxed);
                    self.shared
                        .frames_written
                        .fetch_add(advanced, Ordering::Relaxed);
                }
                Advance::Stale | Advance::NotRunning => break,
            }
        }
        Ok((outcome, written))
    }
}
