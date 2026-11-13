//! Keeps a pixel buffer out of this process's crash dumps while it holds pixels.

use windows_sys::Win32::System::ErrorReporting::{
    WerRegisterExcludedMemoryBlock, WerUnregisterExcludedMemoryBlock,
};

/// One buffer registered with Windows Error Reporting's excluded blocks, until dropped. The
/// owner drops it before it frees or moves the buffer. Registration is in-process only: nothing
/// is written to the registry and nothing outlives the process.
pub(crate) struct DumpExclusion(usize);

impl DumpExclusion {
    /// Registers `len` bytes from `start`, a live allocation's whole capacity. `None` when
    /// it's empty or Windows refuses, which leaves that buffer's pixels in any crash dump written
    /// while it holds them.
    pub(crate) fn new(start: *const u8, len: usize) -> Option<Self> {
        let size = u32::try_from(len).ok().filter(|size| *size > 0)?;
        // SAFETY: records an address and a length; Windows reads neither until a dump.
        let registered = unsafe { WerRegisterExcludedMemoryBlock(start.cast(), size) };
        if registered < 0 {
            tracing::debug!("dump exclusion unavailable");
            return None;
        }
        Some(Self(start.addr()))
    }
}

impl Drop for DumpExclusion {
    fn drop(&mut self) {
        let address = std::ptr::without_provenance::<u8>(self.0);
        // SAFETY: removes the registration this guard made, once.
        let removed = unsafe { WerUnregisterExcludedMemoryBlock(address.cast()) };
        if removed < 0 {
            tracing::debug!("dump exclusion not removed");
        }
    }
}

impl std::fmt::Debug for DumpExclusion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DumpExclusion")
    }
}
