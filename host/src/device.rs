//! The host interface: listing it, reading it, attaching to it and feeding it.

use core::{ffi::c_void, ptr::NonNull, time::Duration};
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};

use hostmic_abi::{
    ABI_VERSION, AdvanceReply, AdvanceRequest, AttachReply, AttachRequest, FormatRange,
    GUID_DEVINTERFACE_HOSTMIC_HOST, IOCTL_ADVANCE, IOCTL_ATTACH, IOCTL_DETACH, IOCTL_GET_INFO,
    IOCTL_GET_STATS, IOCTL_GET_STREAM, Info, Stats, StreamState,
};
use windows::{
    Win32::{
        Devices::DeviceAndDriverInstallation::{
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
            CM_Get_Device_Interface_ListW, CR_BUFFER_SMALL, CR_SUCCESS,
        },
        Foundation::{
            ERROR_BAD_COMMAND, ERROR_BUSY, ERROR_NOT_READY, GENERIC_READ, GENERIC_WRITE, HANDLE,
            WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING,
        },
        System::{
            IO::DeviceIoControl,
            Threading::{CreateEventW, WaitForSingleObject},
        },
    },
    core::{GUID, HSTRING, PCWSTR},
};

use crate::{Error, ring, stream};

/// The paths of the hostmic host interfaces present, one per microphone; empty when the driver
/// is not installed or its device not started.
///
/// # Errors
///
/// [`Error::Os`] when the configuration manager fails, or the list keeps changing.
pub fn interfaces() -> Result<Vec<String>, Error> {
    let guid = GUID::from_values(
        GUID_DEVINTERFACE_HOSTMIC_HOST.data1,
        GUID_DEVINTERFACE_HOSTMIC_HOST.data2,
        GUID_DEVINTERFACE_HOSTMIC_HOST.data3,
        GUID_DEVINTERFACE_HOSTMIC_HOST.data4,
    );
    // The list may grow between the two calls: retry a few times, as Microsoft's samples do.
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
            return Err(configret("CM_Get_Device_Interface_List_SizeW", status.0));
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
            return Ok(buffer
                .split(|&unit| unit == 0)
                .filter(|path| !path.is_empty())
                .map(String::from_utf16_lossy)
                .collect());
        }
        // Only a list that grew is worth another try.
        if status != CR_BUFFER_SMALL {
            return Err(configret("CM_Get_Device_Interface_ListW", status.0));
        }
    }
    Err(configret(
        "CM_Get_Device_Interface_ListW",
        CR_BUFFER_SMALL.0,
    ))
}

/// The path of the first host interface present.
///
/// # Errors
///
/// [`Error::NotInstalled`] when there is none, or [`interfaces`]' errors.
pub fn first_interface() -> Result<String, Error> {
    interfaces()?.into_iter().next().ok_or(Error::NotInstalled)
}

/// A `CONFIGRET` failure as an [`Error::Os`].
fn configret(call: &'static str, status: u32) -> Error {
    Error::Os {
        call,
        error: windows::core::Error::new(
            windows::core::HRESULT(0x8000_4005_u32.cast_signed()),
            format!("CONFIGRET {status:#x}"),
        ),
    }
}

/// A host interface opened for reading: what the driver offers, the stream and the counters.
/// Any process allowed to open the interface may read it, attached host or not.
#[derive(Debug)]
pub struct Device {
    handle: OwnedHandle,
}

impl Device {
    /// Opens the host interface at `path` for reading.
    ///
    /// # Errors
    ///
    /// [`Error::Os`] when `CreateFileW` fails (no such interface, access denied).
    pub fn open(path: &str) -> Result<Self, Error> {
        Ok(Self {
            handle: open(path, GENERIC_READ.0)?,
        })
    }

    /// `GET_INFO`: the driver's interface version, formats and largest buffer.
    ///
    /// # Errors
    ///
    /// [`Error::Os`], or [`Error::Reply`] for a reply of the wrong size.
    pub fn info(&self) -> Result<Info, Error> {
        read(&self.handle, IOCTL_GET_INFO).map(|bytes| Info::from_bytes(&bytes))
    }

    /// `GET_STREAM`: the capture stream's state.
    ///
    /// # Errors
    ///
    /// As [`info`](Self::info).
    pub fn stream(&self) -> Result<StreamState, Error> {
        read(&self.handle, IOCTL_GET_STREAM).map(|bytes| StreamState::from_bytes(&bytes))
    }

    /// `GET_STATS`: the driver's counters.
    ///
    /// # Errors
    ///
    /// As [`info`](Self::info).
    pub fn stats(&self) -> Result<Stats, Error> {
        read(&self.handle, IOCTL_GET_STATS).map(|bytes| Stats::from_bytes(&bytes))
    }
}

/// What [`Host::write`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Advance {
    /// The frames were written and the position advanced to this value.
    Advanced(u64),
    /// The stream changed since it was read (another generation, position or state): nothing
    /// was advanced. Read the stream again, and write from its position.
    Stale,
    /// The stream state given does not accept frames (stopped, paused, or changing): nothing
    /// was written. Wait for the stream's event, then read the stream again. A host whose
    /// stream never runs learns that the driver detached it only at its next write to a
    /// running stream ([`Error::NotHost`]).
    NotRunning,
}

/// The attached host of one microphone: it writes frames into the buffer the driver mapped
/// into this process, and advances the stream position.
///
/// One host at a time per microphone. The attachment ends with [`detach`](Self::detach), or
/// when the `Host` is dropped, which sends `DETACH` and closes the handle: the driver then
/// unmaps the buffer and the microphone shows unplugged. The attachment belongs to this
/// process; a `Host` may move to another thread, which is what a real-time loop does.
///
/// The buffer's mapping is the driver's: it lasts until this process's `DETACH`, the close of
/// every handle to the host's file object, or this process's end
/// ([`AttachReply::buffer`](hostmic_abi::AttachReply::buffer)). The driver does not undo it
/// otherwise: a device removal waits for the handle to close, and it refuses `DETACH` from
/// another process. The handle is not inheritable; only a process with `PROCESS_DUP_HANDLE`
/// on this one could close it from outside. After the driver refuses this handle once
/// ([`Error::NotHost`]), the `Host` writes nothing more.
#[derive(Debug)]
pub struct Host {
    /// Dropped first: closing it detaches and unmaps the buffer if `Drop`'s `DETACH` did not.
    handle: OwnedHandle,
    /// The event the driver signals on every change of the stream.
    event: OwnedHandle,
    /// The buffer's address in this process, page-aligned.
    buffer: NonNull<u8>,
    /// The buffer's size in bytes.
    buffer_bytes: usize,
    /// The formats the microphone offers: the host's that the driver streams.
    formats: FormatRange,
    /// Until `DETACH`, or the driver's first [`Error::NotHost`] for this handle.
    attached: bool,
}

// SAFETY: the mapping is in this process's address space and valid from every thread for as
// long as it lasts, which only this `Host` ends (see the type's documentation). Nothing in
// `Host` is tied to the thread that attached.
unsafe impl Send for Host {}

impl Drop for Host {
    /// Detaches explicitly before the handle closes: closing it alone detaches only when no
    /// other process holds a duplicate of it.
    fn drop(&mut self) {
        if self.attached {
            let _ = call(&self.handle, IOCTL_DETACH, &[], &mut []);
        }
    }
}

impl Host {
    /// Opens the host interface at `path` and attaches as its host, writing formats within
    /// `formats`.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidRange`] for an empty, inverted or unknown range, before anything is
    ///   sent;
    /// - [`Error::Version`] and [`Error::NoCommonFormat`], from the driver's `GET_INFO`,
    ///   before attaching;
    /// - [`Error::Busy`] when another host is attached;
    /// - [`Error::Mapping`] for an unusable buffer (the attachment is then undone);
    /// - [`Error::Os`] and [`Error::Reply`] otherwise.
    pub fn attach(path: &str, formats: FormatRange) -> Result<Self, Error> {
        if !formats.is_valid() {
            return Err(Error::InvalidRange(formats));
        }
        let handle = open(path, GENERIC_READ.0 | GENERIC_WRITE.0)?;
        let info = Info::from_bytes(&read(&handle, IOCTL_GET_INFO)?);
        if info.abi_version != ABI_VERSION {
            return Err(Error::Version {
                driver: info.abi_version,
                host: ABI_VERSION,
            });
        }
        let offered = formats
            .intersect(&info.formats)
            .ok_or(Error::NoCommonFormat {
                host: formats,
                driver: info.formats,
            })?;

        // SAFETY: no security attributes; an unnamed auto-reset event, initially not signaled.
        let event =
            unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.map_err(|error| {
                Error::Os {
                    call: "CreateEventW",
                    error,
                }
            })?;
        // SAFETY: a new handle that nothing else owns.
        let event = unsafe { OwnedHandle::from_raw_handle(event.0) };
        let request = AttachRequest {
            abi_version: ABI_VERSION,
            reserved: 0,
            // Handle values are small: zero-extended, as the interface says.
            event: event.as_raw_handle().addr() as u64,
            formats,
        };
        let mut reply = [0u8; AttachReply::SIZE];
        let returned = match call(&handle, IOCTL_ATTACH, &request.to_bytes(), &mut reply) {
            Ok(returned) => returned,
            Err(error) if error.code() == ERROR_BUSY.to_hresult() => return Err(Error::Busy),
            Err(error) => {
                return Err(Error::Os {
                    call: "ATTACH",
                    error,
                });
            }
        };
        // From here, returning an error drops `handle`, which detaches.
        expect_size("ATTACH", returned, AttachReply::SIZE)?;
        let reply = AttachReply::from_bytes(&reply);
        let unusable = Error::Mapping {
            address: reply.buffer,
            bytes: reply.buffer_bytes,
        };
        let Ok(address) = usize::try_from(reply.buffer) else {
            return Err(unusable);
        };
        if reply.buffer_bytes == 0 || !address.is_multiple_of(4096) {
            return Err(unusable);
        }
        // The driver mapped these pages outside Rust's knowledge: the pointer takes the exposed
        // provenance of memory obtained from the system.
        let Some(buffer) = NonNull::new(core::ptr::with_exposed_provenance_mut::<u8>(address))
        else {
            return Err(unusable);
        };
        Ok(Self {
            handle,
            event,
            buffer,
            buffer_bytes: reply.buffer_bytes as usize,
            formats: offered,
            attached: true,
        })
    }

    /// The formats the microphone offers applications: the host's that the driver streams.
    /// The stream's format, once one is open, is one of them.
    #[must_use]
    pub fn formats(&self) -> FormatRange {
        self.formats
    }

    /// The size of the buffer mapped into this process, in bytes. A stream uses its first
    /// [`StreamState::buffer_frames`] frames.
    #[must_use]
    pub fn buffer_bytes(&self) -> usize {
        self.buffer_bytes
    }

    /// `GET_STREAM`: the capture stream's state. Read it again whenever the stream's event is
    /// signaled ([`wait`](Self::wait)) and after [`Advance::Stale`].
    ///
    /// # Errors
    ///
    /// [`Error::Os`], or [`Error::Reply`] for a reply of the wrong size.
    pub fn stream(&self) -> Result<StreamState, Error> {
        read(&self.handle, IOCTL_GET_STREAM).map(|bytes| StreamState::from_bytes(&bytes))
    }

    /// `GET_STATS`: the driver's counters.
    ///
    /// # Errors
    ///
    /// As [`stream`](Self::stream).
    pub fn stats(&self) -> Result<Stats, Error> {
        read(&self.handle, IOCTL_GET_STATS).map(|bytes| Stats::from_bytes(&bytes))
    }

    /// Waits up to `timeout` (whole milliseconds, at most about 49 days) for the driver to
    /// signal a change of the stream; `true` when it did.
    ///
    /// # Errors
    ///
    /// [`Error::Os`] when the wait fails.
    pub fn wait(&self, timeout: Duration) -> Result<bool, Error> {
        let milliseconds = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        // SAFETY: an event handle this `Host` owns.
        let result = unsafe { WaitForSingleObject(self.event_handle(), milliseconds) };
        if result == WAIT_OBJECT_0 {
            Ok(true)
        } else if result == WAIT_TIMEOUT {
            Ok(false)
        } else {
            Err(Error::Os {
                call: "WaitForSingleObject",
                error: windows::core::Error::from_thread(),
            })
        }
    }

    /// The event the driver signals on every change of the stream (auto-reset), for a loop that
    /// waits on it together with its own objects (a timer, a stop event).
    #[must_use]
    pub fn event(&self) -> BorrowedHandle<'_> {
        self.event.as_handle()
    }

    /// [`event`](Self::event) for the `windows` crate's calls.
    fn event_handle(&self) -> HANDLE {
        HANDLE(self.event.as_raw_handle())
    }

    /// The handle to the host interface, for a removal notification registered on it
    /// (`CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE`). Only the crate sees it: closing or duplicating
    /// it would end the attachment under a writer.
    pub(crate) fn file_handle(&self) -> HANDLE {
        HANDLE(self.handle.as_raw_handle())
    }

    /// Writes `frames` (interleaved samples in `stream`'s format, little-endian, whole frames)
    /// into the ring from stream position `from`, then advances the position past them
    /// (`ADVANCE`).
    ///
    /// `stream` is the state last read ([`stream`](Self::stream)); `from` is its position, or
    /// the position the last [`Advance::Advanced`] returned. Keep writes short, about a
    /// millisecond of audio, and call this right after producing them: the driver takes the
    /// size of the last write as how far the host may be writing ahead of the audio engine.
    ///
    /// No allocation and no lock; one `DeviceIoControl`.
    ///
    /// # Errors
    ///
    /// - [`Error::Write`] for no frame, a partial frame, more than the stream's buffer, or a
    ///   stream buffer larger than the mapping;
    /// - [`Error::NotHost`] when the driver no longer takes this handle as the host (then on
    ///   every later call, without writing);
    /// - [`Error::Os`] and [`Error::Reply`] otherwise.
    pub fn write(
        &mut self,
        stream: &StreamState,
        from: u64,
        frames: &[u8],
    ) -> Result<Advance, Error> {
        if !self.attached {
            return Err(Error::NotHost);
        }
        if !stream::accepts_frames(stream) {
            return Ok(Advance::NotRunning);
        }
        let frame_bytes =
            stream::frame_bytes(&stream.format).ok_or(Error::Write("unknown stream format"))?;
        let frame_size = frame_bytes as usize;
        if frames.is_empty() || !frames.len().is_multiple_of(frame_size) {
            return Err(Error::Write("not a whole number of frames"));
        }
        let count = u32::try_from(frames.len() / frame_size)
            .map_err(|_| Error::Write("more frames than the stream's buffer"))?;
        let ring_bytes = u64::from(stream.buffer_frames) * u64::from(frame_bytes);
        if ring_bytes > self.buffer_bytes as u64 {
            return Err(Error::Write("the stream's buffer exceeds the mapping"));
        }
        let [first, second] = ring::segments(from, count, stream.buffer_frames, frame_bytes)
            .ok_or(Error::Write("more frames than the stream's buffer"))?;
        let (head, tail) = frames.split_at(first.len());
        // SAFETY: both ranges lie within the stream's ring, which fits the mapping (checked
        // above); the mapping lives as long as `self`.
        unsafe { self.copy_to(first.start, head) };
        // SAFETY: as above.
        unsafe { self.copy_to(second.start, tail) };

        let request = AdvanceRequest {
            generation: stream.generation,
            frames: count,
            from,
        };
        let mut reply = [0u8; AdvanceReply::SIZE];
        match call(&self.handle, IOCTL_ADVANCE, &request.to_bytes(), &mut reply) {
            Ok(returned) => {
                expect_size("ADVANCE", returned, AdvanceReply::SIZE)?;
                Ok(Advance::Advanced(AdvanceReply::from_bytes(&reply).position))
            }
            Err(error) if error.code() == ERROR_NOT_READY.to_hresult() => Ok(Advance::Stale),
            Err(error) if error.code() == ERROR_BAD_COMMAND.to_hresult() => {
                self.attached = false;
                Err(Error::NotHost)
            }
            Err(error) => Err(Error::Os {
                call: "ADVANCE",
                error,
            }),
        }
    }

    /// Copies `bytes` into the mapping at `offset`, one volatile store per byte: the audio
    /// engine reads these pages through its own mapping, so they are never accessed through a
    /// Rust reference.
    ///
    /// # Safety
    ///
    /// `offset..offset + bytes.len()` lies within the mapping.
    unsafe fn copy_to(&mut self, offset: usize, bytes: &[u8]) {
        for (index, &byte) in bytes.iter().enumerate() {
            let target = self.buffer.as_ptr().wrapping_add(offset + index);
            // SAFETY: within the mapping, by the caller's contract; a byte has no alignment.
            unsafe { target.write_volatile(byte) };
        }
    }

    /// `DETACH`: ends the attachment and reports its outcome, which dropping the `Host` does
    /// not.
    ///
    /// # Errors
    ///
    /// [`Error::NotHost`] when the driver had already detached this handle, [`Error::Os`]
    /// otherwise. The handle is closed in every case.
    pub fn detach(mut self) -> Result<(), Error> {
        if !self.attached {
            return Err(Error::NotHost);
        }
        self.attached = false;
        match call(&self.handle, IOCTL_DETACH, &[], &mut []) {
            Ok(_) => Ok(()),
            Err(error) if error.code() == ERROR_BAD_COMMAND.to_hresult() => Err(Error::NotHost),
            Err(error) => Err(Error::Os {
                call: "DETACH",
                error,
            }),
        }
    }
}

/// Opens `path` with `access`, sharing it with other readers and the host.
fn open(path: &str, access: u32) -> Result<OwnedHandle, Error> {
    let path = HSTRING::from(path);
    // SAFETY: a NUL-terminated path; no security attributes or template.
    let handle = unsafe {
        CreateFileW(
            &path,
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .map_err(|error| Error::Os {
        call: "CreateFileW",
        error,
    })?;
    // SAFETY: a new handle that nothing else owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
}

/// A request without input whose reply is exactly `N` bytes.
fn read<const N: usize>(handle: &OwnedHandle, code: u32) -> Result<[u8; N], Error> {
    let name = crate::request_name(hostmic_abi::function(code)).unwrap_or("request");
    let mut reply = [0u8; N];
    let returned =
        call(handle, code, &[], &mut reply).map_err(|error| Error::Os { call: name, error })?;
    expect_size(name, returned, N)?;
    Ok(reply)
}

/// [`Error::Reply`] unless `returned` is `expected`.
fn expect_size(request: &'static str, returned: u32, expected: usize) -> Result<(), Error> {
    if returned as usize == expected {
        Ok(())
    } else {
        Err(Error::Reply {
            request,
            returned,
            expected: u32::try_from(expected).unwrap_or(u32::MAX),
        })
    }
}

/// `DeviceIoControl`, synchronous; the bytes returned. Allocates nothing, even on failure.
fn call(
    handle: &OwnedHandle,
    code: u32,
    input: &[u8],
    output: &mut [u8],
) -> windows::core::Result<u32> {
    let mut returned = 0u32;
    // SAFETY: an open handle; buffers of the given lengths that live through the synchronous
    // call; a writable byte count.
    unsafe {
        DeviceIoControl(
            HANDLE(handle.as_raw_handle()),
            code,
            (!input.is_empty()).then_some(input.as_ptr().cast::<c_void>()),
            u32::try_from(input.len()).unwrap_or(u32::MAX),
            (!output.is_empty()).then_some(output.as_mut_ptr().cast::<c_void>()),
            u32::try_from(output.len()).unwrap_or(u32::MAX),
            Some(&raw mut returned),
            None,
        )
    }?;
    Ok(returned)
}
