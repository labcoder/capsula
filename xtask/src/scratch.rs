// The removal shortcut is documented in the shared guard below.
include!("../../tests/support/scratch.rs");

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
        let parent = dir("scratch-label");
        let root = Scratch::under(parent.path(), "root");
        let outside = parent.join("outside");
        fs::write(&outside, b"outside sentinel").unwrap();
        for label in ["../outside", r"part\..\..\outside", "C:", ""] {
            let Err(panic) = catch_unwind(|| Scratch::under(root.path(), label)) else {
                panic!("a path-like label was accepted: {label:?}");
            };
            let reason = panic
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
            assert!(
                reason.is_some_and(|reason| reason.contains("simple name")),
                "{label:?}: {reason:?}"
            );
        }
        assert_eq!(fs::read_dir(&root).expect("no scratch created").count(), 0);
        assert_eq!(
            fs::read(&outside).expect("outside entry survives"),
            b"outside sentinel"
        );
        assert_eq!(
            fs::read_dir(&parent).unwrap().count(),
            2,
            "no outside entries created"
        );
    }
}
