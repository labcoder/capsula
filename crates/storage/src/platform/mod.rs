#[cfg(target_os = "macos")]
#[expect(
    unsafe_code,
    reason = "Darwin descriptor and filesec APIs stay in platform"
)]
pub(crate) mod macos;
#[cfg(windows)]
#[expect(unsafe_code, reason = "Win32 file and security APIs stay in platform")]
pub(crate) mod windows;

/// Why a path can't be trusted. It carries no path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Untrusted {
    /// A path component does not exist.
    #[error("a path component doesn't exist")]
    Missing,
    /// A component is a reparse point or resolves elsewhere.
    #[error("a path component is a reparse point or resolves elsewhere")]
    Reparse,
    /// An untrusted principal can change a component.
    #[error("someone other than this user or the system can change a path component")]
    Unsafe,
    /// A component could not be opened or inspected.
    #[error("a path component can't be opened or read")]
    Io,
}
