//! Loading a pinned native library by full path, and reading the process's module list.

use std::ffi::{CStr, OsString};
use std::fs::File;
use std::io;
use std::marker::PhantomData;
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{ERROR_MOD_NOT_FOUND, FreeLibrary, HMODULE};
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::ProcessStatus::EnumProcessModules;
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// An export's address as the loader returns it; the caller casts it to the declared signature.
pub(crate) type RawExport = unsafe extern "system" fn() -> isize;

/// A loaded DLL, freed on drop. Neither `Send` nor `Sync`: it stays on the thread that loaded it.
pub(crate) struct Library {
    module: HMODULE,
    _not_send: PhantomData<*const ()>,
}

impl Library {
    /// Loads the DLL at the absolute `path`. Its imports bind to modules already loaded under the
    /// same name and otherwise resolve from the system folder only, never from the DLL's own
    /// folder, `PATH` or the current directory.
    pub(crate) fn load(path: &Path) -> io::Result<Self> {
        let path = wide(path);
        // SAFETY: `path` is NUL-terminated and outlives the call; the reserved file handle is null.
        let module = unsafe {
            LoadLibraryExW(
                path.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if module.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            module,
            _not_send: PhantomData,
        })
    }

    /// The named export's address, valid while `self` lives.
    pub(crate) fn export(&self, name: &CStr) -> Option<RawExport> {
        // SAFETY: `module` stays loaded while `self` lives and `name` is NUL-terminated.
        unsafe { GetProcAddress(self.module, name.as_ptr().cast()) }
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: `module` came from LoadLibraryExW and is freed exactly once, here.
        let freed = unsafe { FreeLibrary(self.module) };
        if freed == 0 {
            tracing::warn!(error = %io::Error::last_os_error(), "couldn't free a native library");
        }
    }
}

impl std::fmt::Debug for Library {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Library").finish_non_exhaustive()
    }
}

/// Whether a load failed because one of the DLL's own imports wasn't found.
pub(crate) fn missing_dependency(error: &io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_MOD_NOT_FOUND.cast_signed())
}

/// Opens `path` for reading and refuses writers and deleters while it's open, so bytes checked
/// through this handle are the bytes a load made in that time maps.
pub(crate) fn open_locked(path: &Path) -> io::Result<File> {
    File::options()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
}

/// The full path of every module loaded in this process. A module unloaded meanwhile is left out.
pub(crate) fn modules() -> io::Result<Vec<PathBuf>> {
    // SAFETY: the pseudo-handle for this process needs no cleanup.
    let process = unsafe { GetCurrentProcess() };
    let mut handles: Vec<HMODULE> = vec![std::ptr::null_mut(); 256];
    loop {
        let capacity = u32::try_from(size_of_val(handles.as_slice())).unwrap_or(u32::MAX);
        let mut needed = 0;
        // SAFETY: `handles` is writable for `capacity` bytes and `needed` is a writable u32.
        let listed =
            unsafe { EnumProcessModules(process, handles.as_mut_ptr(), capacity, &raw mut needed) };
        if listed == 0 {
            return Err(io::Error::last_os_error());
        }
        let count = usize::try_from(needed).unwrap_or(usize::MAX) / size_of::<HMODULE>();
        if needed <= capacity {
            handles.truncate(count);
            break;
        }
        handles.resize(count, std::ptr::null_mut());
    }
    Ok(handles.into_iter().filter_map(module_path).collect())
}

fn module_path(module: HMODULE) -> Option<PathBuf> {
    let mut path = vec![0_u16; 32_768];
    let capacity = u32::try_from(path.len()).ok()?;
    // SAFETY: `path` is writable for `capacity` UTF-16 units.
    let len = unsafe { GetModuleFileNameW(module, path.as_mut_ptr(), capacity) };
    if len == 0 || len >= capacity {
        return None;
    }
    path.truncate(usize::try_from(len).ok()?);
    Some(PathBuf::from(OsString::from_wide(&path)))
}

/// The Windows system folder, `System32` on 64-bit Windows.
pub(crate) fn system_dir() -> io::Result<PathBuf> {
    let mut path = vec![0_u16; 1024];
    let capacity = u32::try_from(path.len()).map_err(io::Error::other)?;
    // SAFETY: `path` is writable for `capacity` UTF-16 units.
    let len = unsafe { GetSystemDirectoryW(path.as_mut_ptr(), capacity) };
    if len == 0 || len >= capacity {
        return Err(io::Error::last_os_error());
    }
    path.truncate(usize::try_from(len).map_err(io::Error::other)?);
    Ok(PathBuf::from(OsString::from_wide(&path)))
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
