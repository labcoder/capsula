use std::io;

use kumochi_protocol::constants::ProductIdentity;

/// The brain's launch and lifetime contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The brain runs independently of the shell that starts it.
    Detached,
    /// The shell starts the brain with an inherited pair and owns its lifetime.
    ParentBound,
}

/// Whether this build has the native adapters for the requested mode. Parent-bound is a macOS
/// `first-light` build only, until a held store and an admitted log land.
#[must_use]
pub const fn supports(mode: Mode) -> bool {
    match mode {
        Mode::Detached => cfg!(windows),
        Mode::ParentBound => cfg!(all(target_os = "macos", feature = "first-light")),
    }
}

/// Whether a parent-bound start may take its descriptors as `identity`: the feature is a build
/// switch, and only the Dev edition's names keep it away from another edition's data.
#[must_use]
pub fn admits_parent_bound(identity: ProductIdentity) -> bool {
    supports(Mode::ParentBound) && identity == ProductIdentity::DEV
}

pub(crate) fn require_mode(mode: Mode) -> io::Result<()> {
    if supports(mode) {
        Ok(())
    } else {
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(windows)]
pub(crate) use crate::speech::worker::serve as serve_speech;

#[cfg(not(windows))]
pub(crate) fn serve_speech() -> Result<(), kumochi_protocol::frame::FrameError> {
    Err(io::Error::from(io::ErrorKind::Unsupported).into())
}

#[cfg(test)]
mod tests {
    use kumochi_protocol::constants::ProductIdentity;

    use super::{Mode, admits_parent_bound, supports};

    #[test]
    fn only_a_mac_first_light_build_supports_parent_bound() {
        assert_eq!(supports(Mode::Detached), cfg!(windows));
        assert_eq!(
            supports(Mode::ParentBound),
            cfg!(all(target_os = "macos", feature = "first-light"))
        );
    }

    #[test]
    fn only_the_dev_edition_of_a_supporting_build_admits_parent_bound() {
        let dev = cfg!(all(target_os = "macos", feature = "first-light"));
        for (identity, admitted) in [
            (ProductIdentity::DEV, dev),
            (ProductIdentity::GATE, false),
            (ProductIdentity::RELEASE, false),
        ] {
            assert_eq!(admits_parent_bound(identity), admitted);
        }
    }
}
