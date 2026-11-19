include!("../../tests/support/scratch.rs");

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::ErrorKind;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::AtomicU64;

    use super::{Scratch, dir};

    #[test]
    fn scratch_is_removed_after_success() {
        let path = {
            let scratch = dir("scratch-pass");
            fs::create_dir(scratch.join("nested")).expect("nested folder");
            fs::write(scratch.join("nested/marker"), b"test data").expect("marker");
            scratch.to_path_buf()
        };
        assert_eq!(
            path.symlink_metadata().expect_err("scratch is gone").kind(),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn scratch_is_removed_after_a_panic() {
        let mut path = None;
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let scratch = dir("scratch-unwind");
            fs::write(scratch.join("marker"), b"test data").expect("marker");
            path = Some(scratch.to_path_buf());
            panic!("test body panicked");
        }))
        .expect_err("the test body panicked");
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"test body panicked"));
        assert_eq!(
            path.expect("recorded path")
                .symlink_metadata()
                .expect_err("scratch is gone")
                .kind(),
            ErrorKind::NotFound
        );
    }

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
