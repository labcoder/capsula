//! Protected model storage and Claude Code's private paths.

#[cfg(windows)]
pub(crate) use kumochi_storage::Untrusted;

/// Why a path can't be trusted. It carries no path.
#[cfg(not(windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[expect(
    dead_code,
    reason = "only a native adapter can classify a checked path"
)]
pub(crate) enum Untrusted {
    #[error("a path component doesn't exist")]
    Missing,
    #[error("a path component is a reparse point or resolves elsewhere")]
    Reparse,
    #[error("someone other than this user or the system can change a path component")]
    Unsafe,
    #[error("a path component can't be opened or read")]
    Io,
    #[error("protected paths are unsupported on this platform")]
    Unsupported,
}

/// Whether `error` says this platform has no protected paths at all.
#[cfg(windows)]
pub(crate) fn is_unsupported(_: Untrusted) -> bool {
    false
}

/// Whether `error` says this platform has no protected paths at all.
#[cfg(not(windows))]
pub(crate) fn is_unsupported(error: Untrusted) -> bool {
    error == Untrusted::Unsupported
}

#[cfg(not(windows))]
mod unsupported {
    use std::fs::File;
    use std::path::Path;

    use super::Untrusted;

    pub(crate) fn validate_model_path(_: &Path) -> Result<(), Untrusted> {
        Err(Untrusted::Unsupported)
    }

    pub(crate) fn hold_chain(_: &Path) -> Result<Vec<File>, Untrusted> {
        Err(Untrusted::Unsupported)
    }

    pub(crate) fn owner_only_dir(_: &Path) -> Result<File, Untrusted> {
        Err(Untrusted::Unsupported)
    }

    pub(crate) fn private_tree(_: &Path) -> Result<(), Untrusted> {
        Err(Untrusted::Unsupported)
    }

    pub(crate) fn mark(_: &Path) -> Result<(), Untrusted> {
        Err(Untrusted::Unsupported)
    }

    pub(crate) fn marker(_: &Path) -> Result<Option<File>, Untrusted> {
        Err(Untrusted::Unsupported)
    }

    pub(crate) fn unmark(_: File) -> Result<(), Untrusted> {
        Err(Untrusted::Unsupported)
    }

    pub(crate) fn remove_tree(_: &Path) -> Result<(), Untrusted> {
        Err(Untrusted::Unsupported)
    }

    pub(crate) fn remove_model_tree(_: &Path) -> Result<(), Untrusted> {
        Err(Untrusted::Unsupported)
    }

    pub(crate) fn create_model_dir(_: &Path) -> Result<Vec<File>, Untrusted> {
        Err(Untrusted::Unsupported)
    }

    pub(crate) fn model_file(_: &Path) -> Result<File, Untrusted> {
        Err(Untrusted::Unsupported)
    }

    pub(crate) fn hold_model_creation(_: &Path) -> Result<File, Untrusted> {
        Err(Untrusted::Unsupported)
    }

    pub(crate) fn open_model_write(_: &Path) -> Result<File, Untrusted> {
        Err(Untrusted::Unsupported)
    }

    pub(crate) fn model_final(_: &Path) -> Result<File, Untrusted> {
        Err(Untrusted::Unsupported)
    }

    pub(crate) fn check_model_final(_: &File, _: &Path) -> Result<(), Untrusted> {
        Err(Untrusted::Unsupported)
    }

    pub(crate) fn create_model_write(_: &Path) -> Result<File, Untrusted> {
        Err(Untrusted::Unsupported)
    }

    pub(crate) fn discard_model(_: File) -> Result<(), Untrusted> {
        Err(Untrusted::Unsupported)
    }

    pub(crate) fn publish_model(_: File, _: &File, _: &Path) -> Result<(), Untrusted> {
        Err(Untrusted::Unsupported)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::platform::ModelFiles;

        #[test]
        fn protected_paths_refuse_without_creating_or_mutating_entries() {
            let root = crate::scratch::dir("unsupported-protected");
            let kept = root.join("keep");
            let absent = root.join("absent");
            std::fs::write(&kept, b"fixture").unwrap();
            let file = || File::open(&kept).unwrap();
            let parent = File::open(root.path()).unwrap();

            let model = ModelFiles::open(root.path(), &["keep"]);
            assert_eq!(model.err(), Some(Untrusted::Unsupported));
            assert_eq!(hold_chain(root.path()).err(), Some(Untrusted::Unsupported));
            assert_eq!(marker(&absent).err(), Some(Untrusted::Unsupported));
            assert_eq!(open_model_write(&kept).err(), Some(Untrusted::Unsupported));
            assert_eq!(
                create_model_write(&absent).err(),
                Some(Untrusted::Unsupported)
            );
            assert_eq!(
                create_model_dir(&absent).err(),
                Some(Untrusted::Unsupported)
            );
            assert_eq!(owner_only_dir(&absent).err(), Some(Untrusted::Unsupported));
            for result in [
                mark(&absent),
                check_model_final(&file(), &kept),
                publish_model(file(), &parent, &absent),
                discard_model(file()),
                unmark(file()),
                private_tree(root.path()),
                remove_model_tree(root.path()),
                remove_tree(root.path()),
            ] {
                assert_eq!(result, Err(Untrusted::Unsupported));
            }

            assert_eq!(std::fs::read(&kept).unwrap(), b"fixture");
            assert_eq!(root.read_dir().unwrap().count(), 1);
        }
    }
}

#[cfg(not(windows))]
pub(crate) use unsupported::{
    check_model_final, create_model_dir, create_model_write, discard_model, hold_chain,
    hold_model_creation, mark, marker, model_file, model_final, open_model_write, owner_only_dir,
    private_tree, publish_model, remove_model_tree, remove_tree, unmark, validate_model_path,
};
