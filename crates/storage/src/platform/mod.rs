#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "Win32 calls; storage's only unsafe code lives here"
)]
pub(crate) mod windows;
