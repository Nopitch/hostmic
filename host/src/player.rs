//! The reference loop: a real-time thread that feeds the microphone from a callback.

use core::{
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    time::Duration,
};
use std::{
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    sync::Arc,
    thread::{self, JoinHandle},
};

use hostmic_abi::{Format, SampleType, State, StreamState};
use windows::{
    Win32::{
        Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::{
            Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
            Threading::{
                AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CancelWaitableTimer, CreateEventW,
                CreateWaitableTimerExW, SetEvent, SetWaitableTimer, TIMER_ALL_ACCESS,
                WaitForMultipleObjects,
            },
        },
    },
    core::{HRESULT, PCWSTR, w},
};

use crate::{
    Advance, Error, Host,
    clock::{self, Anchor, Due},
    convert, stream,
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
    /// The stream's state as the player last read it; `None` before the first read.
    pub state: Option<State>,
    /// Whether the thread runs under MMCSS ("Pro Audio").
    pub mmcss: bool,
    /// Whether the thread has ended: stopped, on an error that [`Player::stop`] returns, or on
    /// a panic of the callback, which it resumes.
    pub finished: bool,
}

/// What the player's thread and its owner share.
struct Shared {
    /// Signaled by [`Player::stop`]; manual reset.
    stop: OwnedHandle,
    frequency: i64,
    frames_written: AtomicU64,
    advances: AtomicU64,
    stale: AtomicU64,
    late: AtomicU64,
    longest_gap_ticks: AtomicU64,
    /// A [`State`] code, or `u32::MAX` before the first read.
    state: AtomicU32,
    mmcss: AtomicBool,
    finished: AtomicBool,
}

impl core::fmt::Debug for Shared {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
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
/// It allocates nothing, takes no lock and performs no I/O besides its requests to the driver;
/// the callback must keep that rule too (PLAN.md, "Real-time rule"). It reports through
/// [`stats`](Self::stats), never by printing. The driver's own counters stay readable while it
/// runs, through a [`Device`](crate::Device) opened on the same interface.
#[derive(Debug)]
pub struct Player {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<Result<Host, Error>>>,
}

impl Player {
    /// Starts the loop on its own thread, which takes `host`; `callback` fills each block.
    ///
    /// # Errors
    ///
    /// [`Error::Os`] when an event, the high-resolution timer or the thread cannot be created;
    /// `host` is then dropped, which detaches it.
    pub fn start<F>(host: Host, callback: F) -> Result<Self, Error>
    where
        F: FnMut(&mut Block<'_>) + Send + 'static,
    {
        let mut frequency = 0;
        // SAFETY: a writable frequency.
        unsafe { QueryPerformanceFrequency(&raw mut frequency) }.map_err(|error| Error::Os {
            call: "QueryPerformanceFrequency",
            error,
        })?;
        // SAFETY: no security attributes; an unnamed manual-reset event, not signaled.
        let stop = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(|error| {
            Error::Os {
                call: "CreateEventW",
                error,
            }
        })?;
        // SAFETY: a new handle that nothing else owns.
        let stop = unsafe { OwnedHandle::from_raw_handle(stop.0) };
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
        let shared = Arc::new(Shared {
            stop,
            frequency,
            frames_written: AtomicU64::new(0),
            advances: AtomicU64::new(0),
            stale: AtomicU64::new(0),
            late: AtomicU64::new(0),
            longest_gap_ticks: AtomicU64::new(0),
            state: AtomicU32::new(u32::MAX),
            mmcss: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        });
        // The largest block: the whole mapping in the narrowest sample type.
        let samples = vec![0.0f32; host.buffer_bytes() / 2];
        let bytes = vec![0u8; host.buffer_bytes()];
        let thread_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("hostmic player".to_owned())
            .spawn(move || {
                // Marks the end however the thread ends, a panic of the callback included.
                let _finished = Finished(Arc::clone(&thread_shared));
                let mut work = Work {
                    host,
                    timer,
                    armed: false,
                    shared: thread_shared,
                    samples,
                    bytes,
                };
                work.run(callback).map(|()| work.host)
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
            mmcss: shared.mmcss.load(Ordering::Relaxed),
            finished: shared.finished.load(Ordering::Acquire),
        }
    }

    /// Stops the loop and hands the [`Host`] back, still attached.
    ///
    /// # Errors
    ///
    /// The error that ended the loop before, if any ([`Error::NotHost`] when the driver
    /// detached the host, for one); the `Host` is then dropped.
    ///
    /// # Panics
    ///
    /// Resumes the callback's panic, if it panicked.
    pub fn stop(mut self) -> Result<Host, Error> {
        self.signal_stop();
        let thread = self.thread.take().expect("joined only here or in drop");
        match thread.join() {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    fn signal_stop(&self) {
        // SAFETY: an event handle the shared state owns, valid while `self` lives. It cannot
        // fail for a valid event handle.
        let _ = unsafe { SetEvent(HANDLE(self.shared.stop.as_raw_handle())) };
    }
}

impl Drop for Player {
    /// Stops the loop; the host is dropped with it, which detaches it.
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.signal_stop();
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

/// The player thread's state.
struct Work {
    host: Host,
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
    /// Registers with MMCSS and runs [`Work::serve`].
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
        self.serve(&mut callback)
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

    /// The loop itself, until the stop event or an error.
    fn serve<F: FnMut(&mut Block<'_>)>(&mut self, callback: &mut F) -> Result<(), Error> {
        let handles = [
            HANDLE(self.shared.stop.as_raw_handle()),
            HANDLE(self.host.event().as_raw_handle()),
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
                stream = self.host.stream()?;
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
            // SAFETY: three handles this thread or its owner keeps open for the whole loop.
            let wait = unsafe { WaitForMultipleObjects(&handles, false, 100) };
            if wait == WAIT_OBJECT_0 {
                return Ok(());
            } else if wait.0 == WAIT_OBJECT_0.0 + 1 {
                read = true;
                continue;
            } else if wait == WAIT_FAILED {
                return Err(Error::Os {
                    call: "WaitForMultipleObjects",
                    error: windows::core::Error::from_thread(),
                });
            } else if wait != WAIT_TIMEOUT && wait.0 != WAIT_OBJECT_0.0 + 2 {
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
            outcome = self.host.write(stream, at, &self.bytes[first..end])?;
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
