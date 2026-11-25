use std::ffi::OsString;

#[cfg(unix)]
pub(super) fn non_unicode() -> OsString {
    use std::os::unix::ffi::OsStringExt as _;
    OsString::from_vec(vec![0xff])
}

#[cfg(windows)]
pub(super) fn non_unicode() -> OsString {
    use std::os::windows::ffi::OsStringExt as _;
    OsString::from_wide(&[0xd800])
}
