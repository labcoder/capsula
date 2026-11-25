// The removal shortcut is documented in the shared guard below.
include!("../../tests/support/scratch.rs");

/// Scratch in the real local application data folder, for a test whose folders must pass a chain
/// hold: the Temp chain may not, depending on how the machine's folders were made.
#[cfg(windows)]
pub(crate) fn local(label: &str) -> Scratch {
    let local = crate::platform::local_app_data().expect("local app data");
    Scratch::under(&local, label)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::panic::catch_unwind;
    use std::sync::atomic::AtomicU64;

    use super::{Scratch, dir};

    #[test]
    fn a_preexisting_folder_is_left_alone() {
        let root = dir("scratch-collision");
        let old = Scratch::numbered(root.path(), "same", &AtomicU64::new(0));
        let marker = old.join("marker");
        fs::write(&marker, b"preexisting data").expect("marker");

        let fresh = Scratch::numbered(root.path(), "same", &AtomicU64::new(0));
        assert_ne!(fresh.path(), old.path());
        assert_eq!(fs::read_dir(&fresh).expect("empty scratch").count(), 0);
        drop(fresh);
        assert_eq!(
            fs::read(marker).expect("marker survives"),
            b"preexisting data"
        );
    }

    #[test]
    fn a_label_cannot_introduce_a_path() {
        let root = dir("scratch-label");
        for label in ["../outside", r"part\..\..\outside", "C:", ""] {
            let Err(panic) = catch_unwind(|| Scratch::under(root.path(), label)) else {
                panic!("a path-like label was accepted");
            };
            assert_eq!(
                panic.downcast_ref::<&str>(),
                Some(&"a scratch label must be a simple name")
            );
        }
        assert_eq!(fs::read_dir(&root).expect("no scratch created").count(), 0);
    }
}
