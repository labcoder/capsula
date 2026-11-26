//! macOS candidates for shell-owned platform resources.

#[expect(
    unsafe_code,
    reason = "Darwin process calls are isolated to this platform adapter"
)]
mod child;
#[expect(
    unsafe_code,
    reason = "Darwin resource-limit calls are isolated to this platform adapter"
)]
mod core_dumps;
mod instance;
mod leg;
#[cfg(test)]
mod link_tests;
mod pair;
#[cfg(any(test, feature = "test-support"))]
mod wipes;

pub(crate) use child::{NotStarted, start_brain};
pub(crate) use core_dumps::limit_core_dumps;
pub(crate) use instance::{read_instance, try_acquire_instance_lock};
pub use kumochi_storage::application_support_dir as support_dir;
pub(crate) use leg::{PipeClient, Secret, closer, initiate, read_ready};
pub(crate) use pair::ShellEnds;

/// The shell's session for the link: always 0. The Mac's instance item and lock are per user, not
/// per login session, so nothing on the pair path is named by it; a second login's brain exits
/// Busy instead.
///
/// # Errors
/// Never; the signature is the platforms' shared one.
pub fn session_id() -> std::io::Result<u32> {
    Ok(0)
}

/// The session channel of this build's edition. Gate identity has no wire value, so it refuses.
pub(crate) fn channel() -> std::io::Result<kumochi_session::Channel> {
    channel_of(kumochi_protocol::constants::ProductIdentity::ACTIVE)
}

fn channel_of(
    identity: kumochi_protocol::constants::ProductIdentity,
) -> std::io::Result<kumochi_session::Channel> {
    use kumochi_protocol::constants::ProductIdentity;
    match identity {
        identity if identity == ProductIdentity::RELEASE => Ok(kumochi_session::Channel::Release),
        identity if identity == ProductIdentity::DEV => Ok(kumochi_session::Channel::Dev),
        _ => Err(std::io::ErrorKind::Unsupported.into()),
    }
}

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
    fn the_session_is_0_and_a_link_outside_a_bundle_has_no_brain() {
        assert_eq!(super::session_id().expect("no session to read"), 0);
        let (events, _) = async_channel::unbounded();
        let prepared =
            crate::link::prepare(events, kumochi_protocol::message::AudioSession::Legacy);
        assert_eq!(
            prepared.map(drop).map_err(|error| error.kind()),
            Err(std::io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn each_identity_has_its_own_channel_and_gate_has_none() {
        use kumochi_protocol::constants::ProductIdentity;
        use kumochi_session::Channel;
        let mut active = 0;
        for (identity, channel) in [
            (ProductIdentity::RELEASE, Ok(Channel::Release)),
            (ProductIdentity::DEV, Ok(Channel::Dev)),
            (ProductIdentity::GATE, Err(std::io::ErrorKind::Unsupported)),
        ] {
            assert_eq!(
                super::channel_of(identity).map_err(|error| error.kind()),
                channel,
                "{identity:?}"
            );
            if identity == ProductIdentity::ACTIVE {
                active += 1;
                assert_eq!(
                    super::channel().map_err(|error| error.kind()),
                    channel,
                    "the build's own"
                );
            }
        }
        assert_eq!(active, 1, "the build's identity is one of the three");
    }

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
