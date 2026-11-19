//! Read-only capture metadata and control of this harness's uniquely named ETW session.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::File;
use std::io::Read as _;
use std::os::windows::io::AsRawHandle as _;
use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, ensure};
use windows::Win32::Foundation::ERROR_WMI_INSTANCE_NOT_FOUND;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
use windows::Win32::Security::Cryptography::{
    BCRYPT_ALG_HANDLE, BCRYPT_HASH_HANDLE, BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS,
    BCRYPT_SHA256_ALGORITHM, BCryptCloseAlgorithmProvider, BCryptCreateHash, BCryptDestroyHash,
    BCryptFinishHash, BCryptHashData, BCryptOpenAlgorithmProvider,
};
use windows::Win32::Storage::FileSystem::{
    FILE_NAME_NORMALIZED, FILE_TYPE_DISK, GetFileType, GetFinalPathNameByHandleW,
};
use windows::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
use windows::Win32::System::Diagnostics::Etw::{
    CONTROLTRACE_HANDLE, ControlTraceW, EVENT_TRACE_CONTROL, EVENT_TRACE_CONTROL_QUERY,
    EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_PROPERTIES,
};
use windows::Win32::System::Performance::QueryPerformanceCounter;
use windows::Win32::System::Performance::QueryPerformanceFrequency;
use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
use windows::Win32::System::Threading::GetProcessTimes;
use windows::Win32::System::Threading::{CREATE_NO_WINDOW, NORMAL_PRIORITY_CLASS};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CMONITORS, SM_CXSCREEN, SM_CYSCREEN,
};
use windows::core::HSTRING;

#[derive(Debug)]
pub(crate) struct CreationClock {
    pub(crate) earliest: i64,
    pub(crate) latest: i64,
    pub(crate) frequency: i64,
    pub(crate) evidence: String,
}

pub(crate) fn foreground() -> usize {
    // SAFETY: this reads a numeric foreground handle only, never window content.
    unsafe { GetForegroundWindow() }.0.addr()
}

pub(crate) fn creation_clock(process: &super::Process) -> Result<CreationClock> {
    let mut frequency = 0;
    // SAFETY: frequency is a writable counter-frequency output.
    unsafe { QueryPerformanceFrequency(&raw mut frequency) }?;
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: the owned process handle is live with query access; all outputs are writable.
    unsafe {
        GetProcessTimes(
            HANDLE(process.handle.as_raw_handle()),
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    }?;
    let before = qpc()?;
    // SAFETY: this read-only clock API returns the current precise UTC FILETIME.
    let utc = unsafe { GetSystemTimePreciseAsFileTime() };
    let after = qpc()?;
    let value =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    let created = value(created);
    let utc = value(utc);
    let (earliest, latest) = map_creation(created, utc, before, after, frequency)?;
    ensure!(
        latest - earliest <= frequency / 1000,
        "UTC/QPC anchor exceeds1ms uncertainty"
    );
    Ok(CreationClock {
        earliest,
        latest,
        frequency,
        evidence: format!(
            "os_creation_filetime={created} anchor_filetime={utc} anchor_qpc_before={before} anchor_qpc_after={after} frequency={frequency} creation_qpc_lower={earliest} creation_qpc_upper={latest}; OS-reported UTC creation mapped through a preciseUTC/QPC bracket\n"
        ),
    })
}

fn map_creation(
    created: u64,
    utc: u64,
    before: i64,
    after: i64,
    frequency: i64,
) -> Result<(i64, i64)> {
    ensure!(
        frequency > 0 && before > 0 && after >= before,
        "invalid creation clock bracket"
    );
    let age = utc
        .checked_sub(created)
        .context("process creation is in the future; UTC clock changed")?;
    let scaled = u128::from(age) * u128::try_from(frequency)?;
    let early = before
        .checked_sub(i64::try_from(scaled.div_ceil(10_000_000))?)
        .context("creation QPC underflow")?;
    let late = after
        .checked_sub(i64::try_from(scaled / 10_000_000)?)
        .context("creation QPC underflow")?;
    ensure!(
        early > 0 && late >= early,
        "invalid mapped process creation"
    );
    Ok((early, late))
}

fn final_path(handle: HANDLE) -> Result<PathBuf> {
    // SAFETY: the caller supplies a live borrowed file handle; no ownership is transferred.
    let kind = unsafe { GetFileType(handle) };
    ensure!(
        kind == FILE_TYPE_DISK,
        "gate transcript must be an actual disk file"
    );
    let mut name = vec![0; 32768];
    // SAFETY: name is a writable UTF-16 buffer, and the borrowed handle stays live.
    let length = unsafe { GetFinalPathNameByHandleW(handle, &mut name, FILE_NAME_NORMALIZED) };
    let length = usize::try_from(length)?;
    ensure!(
        length > 0 && length < name.len(),
        "can't resolve gate transcript file handle"
    );
    Ok(PathBuf::from(String::from_utf16(&name[..length])?))
}

pub(crate) fn require_transcript(path: &Path) -> Result<()> {
    let expected = path
        .canonicalize()
        .context("resolving requested retained transcript")?;
    for stream in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: this borrows one standard stream handle; it must never be closed here.
        let handle = unsafe { GetStdHandle(stream) }?;
        ensure!(
            final_path(handle)? == expected,
            "stdout and stderr must both be redirected to the requested transcript"
        );
    }
    Ok(())
}

pub(crate) fn qpc() -> Result<i64> {
    let mut counter = 0;
    // SAFETY: counter is a writable QPC output.
    unsafe { QueryPerformanceCounter(&raw mut counter) }?;
    ensure!(counter > 0, "invalid performance counter");
    Ok(counter)
}

fn presentmon_path(path: Option<OsString>) -> Result<PathBuf> {
    let path = PathBuf::from(path.context("set PRESENTMON_EXE to your PresentMon-2.6.0-x64.exe")?);
    ensure!(
        path.is_absolute(),
        "PRESENTMON_EXE must be an absolute path"
    );
    ensure!(
        path.file_name().and_then(|name| name.to_str()) == Some("PresentMon-2.6.0-x64.exe"),
        "PRESENTMON_EXE must select PresentMon-2.6.0-x64.exe"
    );
    Ok(path)
}

pub(crate) fn presentmon_command() -> Result<Command> {
    let path = presentmon_path(std::env::var_os("PRESENTMON_EXE"))?;
    ensure!(path.is_file(), "PRESENTMON_EXE must name an existing file");
    let mut command = Command::new(path);
    command.creation_flags((CREATE_NO_WINDOW | NORMAL_PRIORITY_CLASS).0);
    Ok(command)
}

pub(crate) fn desktop_metadata() -> Result<String> {
    let mut mode = DEVMODEW {
        dmSize: u16::try_from(size_of::<DEVMODEW>())?,
        ..Default::default()
    };
    // SAFETY: mode has its documented writable size; a null device queries the current primary display.
    unsafe { EnumDisplaySettingsW(None, ENUM_CURRENT_SETTINGS, &raw mut mode) }.ok()?;
    // SAFETY: these read-only numeric system metrics require no pointers or handles.
    let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    // SAFETY: this reads one numeric system metric without pointers or handles.
    let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    // SAFETY: this reads one numeric system metric without pointers or handles.
    let monitors = unsafe { GetSystemMetrics(SM_CMONITORS) };
    ensure!(
        width > 0 && height > 0 && mode.dmDisplayFrequency > 1,
        "invalid current display metadata"
    );
    let output = Command::new("tasklist.exe")
        .args(["/FO", "CSV"])
        .creation_flags(CREATE_NO_WINDOW.0)
        .output()
        .context("reading background process snapshot")?;
    ensure!(
        output.status.success(),
        "background process snapshot failed: {}",
        output.status
    );
    Ok(format!(
        "primary_width_px={width} primary_height_px={height} monitors={monitors} refresh_hz={}\nprocess_snapshot_exit={}\n{}",
        mode.dmDisplayFrequency,
        output.status,
        String::from_utf8_lossy(&output.stdout)
    ))
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TraceLoss {
    pub(crate) events: u32,
    pub(crate) log_buffers: u32,
    pub(crate) realtime_buffers: u32,
}

impl TraceLoss {
    pub(crate) fn validate(self) -> Result<()> {
        ensure!(
            self.events == 0 && self.log_buffers == 0 && self.realtime_buffers == 0,
            "owned trace lost data: {self:?}"
        );
        Ok(())
    }
}

#[repr(C)]
struct TraceQuery {
    properties: EVENT_TRACE_PROPERTIES,
    name: [u16; 1024],
    logfile: [u16; 1024],
}

fn control(name: &str, operation: EVENT_TRACE_CONTROL) -> Result<Option<TraceLoss>> {
    ensure!(
        !name.is_empty() && name.encode_utf16().count() < 1024 && !name.contains('\0'),
        "invalid owned trace name"
    );
    let mut buffer = TraceQuery {
        properties: EVENT_TRACE_PROPERTIES::default(),
        name: [0; 1024],
        logfile: [0; 1024],
    };
    buffer.properties.Wnode.BufferSize = u32::try_from(size_of::<TraceQuery>())?;
    buffer.properties.LoggerNameOffset = u32::try_from(std::mem::offset_of!(TraceQuery, name))?;
    buffer.properties.LogFileNameOffset = u32::try_from(std::mem::offset_of!(TraceQuery, logfile))?;
    // SAFETY: buffer is aligned, writable, and includes both documented string buffers; only our named session is addressed.
    let status = unsafe {
        ControlTraceW(
            CONTROLTRACE_HANDLE::default(),
            &HSTRING::from(name),
            &raw mut buffer.properties,
            operation,
        )
    };
    if status == ERROR_WMI_INSTANCE_NOT_FOUND {
        return Ok(None);
    }
    status.ok()?;
    let properties = buffer.properties;
    Ok(Some(TraceLoss {
        events: properties.EventsLost,
        log_buffers: properties.LogBuffersLost,
        realtime_buffers: properties.RealTimeBuffersLost,
    }))
}

pub(crate) fn trace_stats(name: &str) -> Result<Option<TraceLoss>> {
    control(name, EVENT_TRACE_CONTROL_QUERY)
}

pub(crate) fn stop_trace(name: &str) -> Result<TraceLoss> {
    control(name, EVENT_TRACE_CONTROL_STOP)?
        .context("owned trace ended before final loss statistics")
}

pub(crate) fn file_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut algorithm = BCRYPT_ALG_HANDLE::default();
    // SAFETY: the algorithm handle output is writable and the algorithm identifier is a static Win32 string.
    unsafe {
        BCryptOpenAlgorithmProvider(
            &raw mut algorithm,
            BCRYPT_SHA256_ALGORITHM,
            None,
            BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS::default(),
        )
    }
    .ok()?;
    let mut hash = BCRYPT_HASH_HANDLE::default();
    let result = (|| -> Result<String> {
        // SAFETY: algorithm is live; null object storage requests Windows-owned hash storage, supported since Windows 7.
        unsafe { BCryptCreateHash(algorithm, &raw mut hash, None, None, 0) }.ok()?;
        let mut bytes = vec![0; 64 * 1024];
        loop {
            let count = file.read(&mut bytes)?;
            if count == 0 {
                break;
            }
            // SAFETY: hash is live and the byte slice contains only the file bytes read above.
            unsafe { BCryptHashData(hash, &bytes[..count], 0) }.ok()?;
        }
        let mut digest = [0; 32];
        // SAFETY: SHA-256 produces exactly the writable 32-byte digest supplied here.
        unsafe { BCryptFinishHash(hash, &mut digest, 0) }.ok()?;
        let mut output = String::with_capacity(64);
        for byte in digest {
            write!(output, "{byte:02x}")?;
        }
        Ok(output)
    })();
    let destroyed = if hash.0.is_null() {
        Ok(())
    } else {
        // SAFETY: only the unique hash handle created above is destroyed.
        unsafe { BCryptDestroyHash(hash) }.ok()
    };
    // SAFETY: only the algorithm handle owned by this function is closed.
    let closed = unsafe { BCryptCloseAlgorithmProvider(algorithm, 0) }.ok();
    match (result, destroyed, closed) {
        (Ok(digest), Ok(()), Ok(())) => Ok(digest),
        (result, destroyed, closed) => Err(anyhow::anyhow!(
            "hash result={:?}; destroying hash={destroyed:?}; closing algorithm={closed:?}",
            result.err()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{TraceLoss, file_sha256, final_path, map_creation, presentmon_path};
    use std::os::windows::io::AsRawHandle as _;

    #[test]
    fn presentmon_selection_requires_an_explicit_absolute_pinned_path() {
        let directory = std::env::temp_dir().join("chosen tools with spaces");
        let selected = directory.join("PresentMon-2.6.0-x64.exe");
        assert_eq!(
            presentmon_path(Some(selected.clone().into_os_string())).unwrap(),
            selected
        );
        assert!(presentmon_path(None).is_err());
        for path in [
            std::path::PathBuf::new(),
            "PresentMon-2.6.0-x64.exe".into(),
            "tools/PresentMon-2.6.0-x64.exe".into(),
            directory.join("PresentMon-2.5.0-x64.exe"),
            directory.join("PresentMon-2.6.0-x86.exe"),
            directory.join("PresentMon.exe"),
            directory.join("other.exe"),
        ] {
            assert!(presentmon_path(Some(path.into_os_string())).is_err());
        }
    }

    #[test]
    fn creation_utc_maps_to_a_bounded_qpc_interval() {
        assert_eq!(
            map_creation(100_000_000, 110_000_000, 2_000_000, 2_000_010, 1_000_000).unwrap(),
            (1_000_000, 1_000_010)
        );
        assert_eq!(map_creation(10, 11, 100, 101, 3).unwrap(), (99, 101));
        assert!(map_creation(12, 11, 100, 101, 1000).is_err());
        assert!(map_creation(10, 11, 101, 100, 1000).is_err());
    }

    #[test]
    fn file_handle_resolves_to_the_retained_transcript_path() {
        let directory = crate::scratch::dir("transcript-test");
        let path = directory.join("transcript");
        let file = std::fs::File::create_new(&path).unwrap();
        let resolved = final_path(super::HANDLE(file.as_raw_handle()));
        let expected = path.canonicalize().unwrap();
        assert_eq!(resolved.unwrap(), expected);
    }

    #[test]
    fn sha256_matches_standard_empty_file_vector() {
        let directory = crate::scratch::dir("hash-test");
        let path = directory.join("empty");
        drop(std::fs::File::create_new(&path).unwrap());
        let result = file_sha256(&path);
        assert_eq!(
            result.unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn any_trace_loss_invalidates_the_capture() {
        assert!(
            TraceLoss {
                events: 0,
                log_buffers: 0,
                realtime_buffers: 0
            }
            .validate()
            .is_ok()
        );
        for stats in [
            TraceLoss {
                events: 1,
                log_buffers: 0,
                realtime_buffers: 0,
            },
            TraceLoss {
                events: 0,
                log_buffers: 1,
                realtime_buffers: 0,
            },
            TraceLoss {
                events: 0,
                log_buffers: 0,
                realtime_buffers: 1,
            },
        ] {
            assert!(stats.validate().is_err());
        }
    }
}
