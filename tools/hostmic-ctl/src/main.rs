//! `hostmic-ctl`: inspects the hostmic driver through its host interface.
//!
//! ```text
//! hostmic-ctl info     every host interface: its path and what the driver offers
//! ```
//!
//! Plain text, one value per line with its unit, in the style of Virtual Audio Cable's control
//! panel (PLAN.md, "Host library"). Exit status: 0 on success, 1 when no interface answers, 2 on
//! a usage error.

use std::{env, process::ExitCode};

use hostmic_abi::{GUID_DEVINTERFACE_HOSTMIC_HOST, IOCTL_GET_INFO, Info, SampleType};
use windows::{
    Win32::{
        Devices::DeviceAndDriverInstallation::{
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
            CM_Get_Device_Interface_ListW, CR_SUCCESS,
        },
        Foundation::{CloseHandle, GENERIC_READ, HANDLE},
        Storage::FileSystem::{
            CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING,
        },
        System::IO::DeviceIoControl,
    },
    core::{GUID, HSTRING, PCWSTR},
};

const USAGE: &str = "usage: hostmic-ctl info";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["info"] | [] => info(),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// `hostmic-ctl info`.
fn info() -> ExitCode {
    let paths = match interface_paths() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("cannot list host interfaces: {error}");
            return ExitCode::FAILURE;
        }
    };
    if paths.is_empty() {
        eprintln!("no hostmic host interface: is the driver installed and started?");
        return ExitCode::FAILURE;
    }
    let mut failed = false;
    for path in &paths {
        println!("hostmic host interface");
        println!("  path          {path}");
        match Device::open(path).and_then(|device| device.info()) {
            Ok(info) => print_info(&info),
            Err(error) => {
                println!("  error         {error}");
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn print_info(info: &Info) {
    let formats = info.formats;
    println!("  abi version   {}", info.abi_version);
    println!("  max buffer    {} bytes", info.max_buffer_bytes);
    println!(
        "  rates         {}",
        range(formats.min_rate, formats.max_rate, "Hz")
    );
    println!(
        "  channels      {}",
        range(
            u32::from(formats.min_channels),
            u32::from(formats.max_channels),
            "ch"
        )
    );
    println!("  sample types  {}", sample_types(formats.sample_types));
}

/// `48000 Hz`, or `44100..96000 Hz`.
fn range(min: u32, max: u32, unit: &str) -> String {
    if min == max {
        format!("{min} {unit}")
    } else {
        format!("{min}..{max} {unit}")
    }
}

/// The sample types of a [`hostmic_abi::FormatRange::sample_types`] mask, by name.
fn sample_types(mask: u32) -> String {
    let names: Vec<&str> = SampleType::ALL
        .into_iter()
        .filter(|sample_type| mask & sample_type.bit() != 0)
        .map(|sample_type| match sample_type {
            SampleType::Int16 => "int16",
            SampleType::Int24 => "int24",
            SampleType::Int32 => "int32",
            SampleType::Float32 => "float32",
        })
        .collect();
    if names.is_empty() {
        format!("none ({mask:#x})")
    } else {
        names.join(", ")
    }
}

/// The paths of the present host interfaces.
fn interface_paths() -> Result<Vec<String>, String> {
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
            return Err(format!(
                "CM_Get_Device_Interface_List_SizeW: CONFIGRET {}",
                status.0
            ));
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
    }
    Err("the interface list kept changing".to_owned())
}

/// An open host interface.
struct Device(HANDLE);

impl Device {
    /// Opens `path` for reading: enough for the requests that only read.
    fn open(path: &str) -> Result<Self, windows::core::Error> {
        let path = HSTRING::from(path);
        // SAFETY: a NUL-terminated path; no security attributes or template.
        let handle = unsafe {
            CreateFileW(
                &path,
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }?;
        Ok(Self(handle))
    }

    /// `IOCTL_GET_INFO`.
    fn info(&self) -> Result<Info, windows::core::Error> {
        let mut reply = [0u8; Info::SIZE];
        let mut returned = 0u32;
        // SAFETY: an open handle; no input; an output buffer of `Info::SIZE` bytes that lives
        // through the synchronous call; a writable byte count.
        unsafe {
            DeviceIoControl(
                self.0,
                IOCTL_GET_INFO,
                None,
                0,
                Some(reply.as_mut_ptr().cast()),
                u32::try_from(reply.len()).unwrap_or(u32::MAX),
                Some(&raw mut returned),
                None,
            )
        }?;
        if returned as usize != Info::SIZE {
            return Err(windows::core::Error::new(
                windows::core::HRESULT(0x8000_FFFF_u32.cast_signed()),
                format!(
                    "GET_INFO returned {returned} bytes, expected {}",
                    Info::SIZE
                ),
            ));
        }
        Ok(Info::from_bytes(&reply))
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: a handle this value owns, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}
