//! macOS candidates for shell-owned platform resources.

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the Mac connection starts its brain child from I1e-2"
    )
)]
#[expect(
    unsafe_code,
    reason = "Darwin process calls are isolated to this platform adapter"
)]
mod child;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the link doesn't connect over the pair until I1e-5"
    )
)]
#[expect(
    unsafe_code,
    reason = "Darwin resource-limit calls are isolated to this platform adapter"
)]
mod core_dumps;
mod instance;
mod leg;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the Mac connection starts its brain child from I1e-2"
    )
)]
mod pair;
#[expect(
    dead_code,
    reason = "Mac capture reads the permission from its own pull request"
)]
#[expect(
    unsafe_code,
    reason = "reads the microphone permission through AVFAudio"
)]
pub(crate) mod permission;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Mac capture fills the ring from its own pull request"
    )
)]
#[expect(
    unsafe_code,
    reason = "the ring's slots are shared with the CoreAudio IO thread"
)]
mod ring;
#[cfg(any(test, feature = "test-support"))]
mod wipes;

pub(crate) use child::{NotStarted, start_brain};
pub(crate) use core_dumps::limit_core_dumps;
pub(crate) use instance::{read_instance, try_acquire_instance_lock};
pub use kumochi_storage::application_support_dir as support_dir;
pub(crate) use leg::{PipeClient, Secret, initiate, read_ready};
pub(crate) use pair::ShellEnds;

/// The brain helper nested in the shell's bundle: from an absolute
/// `<name>.app/Contents/MacOS/<shell>` up to `Contents`, then down
/// [`BRAIN_HELPER`](kumochi_protocol::constants::BRAIN_HELPER).
///
/// # Errors
/// `NotFound` when `exe` isn't an absolute path inside that layout.
pub(crate) fn brain_helper(exe: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    use std::ffi::OsStr;
    use std::path::Path;
    let macos = exe.parent().filter(|_| exe.is_absolute());
    let contents = macos
        .filter(|dir| dir.file_name() == Some(OsStr::new("MacOS")))
        .and_then(Path::parent)
        .filter(|dir| dir.file_name() == Some(OsStr::new("Contents")));
    contents
        .filter(|dir| dir.parent().and_then(Path::extension) == Some(OsStr::new("app")))
        .map(|contents| contents.join(kumochi_protocol::constants::BRAIN_HELPER))
        .ok_or_else(|| std::io::ErrorKind::NotFound.into())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use kumochi_protocol::constants::BRAIN_EXE;

    #[test]
    fn the_brain_is_the_helper_nested_in_the_shell_bundle() {
        let exe = Path::new("/X/Kumochi.app/Contents/MacOS/kumochi");
        let expected = PathBuf::from(format!(
            "/X/Kumochi.app/Contents/Helpers/{BRAIN_EXE}.app/Contents/MacOS/{BRAIN_EXE}"
        ));
        assert_eq!(
            expected,
            Path::new("/X/Kumochi.app/Contents/Helpers/kumochid.app/Contents/MacOS/kumochid")
        );
        assert_eq!(super::brain_helper(exe).expect("resolved"), expected);
    }

    #[test]
    fn an_absolute_exe_outside_an_app_bundle_has_no_helper() {
        for exe in [
            "/kumochi",
            "/tmp/tools/kumochi",
            "/X/Kumochi/Contents/MacOS/kumochi",
            "/X/Kumochi.app/Contents/Resources/kumochi",
            "/X/Kumochi.app/Other/MacOS/kumochi",
            "/X/.app/Contents/MacOS/kumochi",
        ] {
            assert_eq!(
                super::brain_helper(Path::new(exe)).map_err(|error| error.kind()),
                Err(std::io::ErrorKind::NotFound),
                "{exe}"
            );
        }
    }

    #[test]
    fn a_relative_exe_has_no_helper_even_in_the_bundle_layout() {
        assert_eq!(
            super::brain_helper(Path::new("Kumochi.app/Contents/MacOS/kumochi"))
                .map_err(|error| error.kind()),
            Err(std::io::ErrorKind::NotFound)
        );
    }
}
