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

pub(crate) use child::{NotStarted, start_brain};
pub(crate) use instance::try_acquire_instance_lock;
pub use kumochi_storage::application_support_dir as support_dir;
pub(crate) use leg::{PipeClient, initiate};
pub(crate) use pair::ShellEnds;

/// The brain helper nested in the shell's bundle: from `Contents/MacOS/<shell>` up to
/// `Contents`, then down [`BRAIN_HELPER`](kumochi_protocol::constants::BRAIN_HELPER).
///
/// # Errors
/// `NotFound` when `exe` has no `Contents` folder above it.
pub(crate) fn brain_helper(exe: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    exe.parent()
        .and_then(std::path::Path::parent)
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
    fn an_exe_without_a_contents_folder_has_no_helper() {
        assert_eq!(
            super::brain_helper(Path::new("/kumochi"))
                .expect_err("nothing above the root")
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }
}
