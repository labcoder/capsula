use std::fs;
use std::io::ErrorKind;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A fresh directory owned by one test, removed when the guard goes out of scope.
#[must_use = "the scratch directory only lives while its guard is held"]
pub(crate) struct Scratch {
    path: PathBuf,
    owned: PathBuf,
}

pub(crate) fn dir(label: &str) -> Scratch {
    Scratch::under(&std::env::temp_dir(), label)
}

impl Scratch {
    pub(crate) fn under(root: &Path, label: &str) -> Self {
        Self::numbered(root, label, &NEXT)
    }

    fn numbered(root: &Path, label: &str, next: &AtomicU64) -> Self {
        assert!(
            !label.is_empty()
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b' ')),
            "a scratch label must be a simple name"
        );
        let root = std::path::absolute(root).expect("an absolute scratch root");
        let resolved = root.canonicalize().expect("an existing scratch root");
        loop {
            let number = next.fetch_add(1, Ordering::Relaxed);
            let name = format!(
                "{}-{label}-{}-{number}",
                kumochi_protocol::constants::OBJECT_PREFIX,
                std::process::id()
            );
            let owned = resolved.join(&name);
            match fs::create_dir(&owned) {
                Ok(()) => {
                    return Self {
                        path: root.join(name),
                        owned,
                    };
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => panic!("could not create a scratch directory: {}", error.kind()),
            }
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        self.path()
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match fs::remove_dir_all(&self.owned) {
                Ok(()) => return,
                Err(error) if error.kind() == ErrorKind::NotFound => return,
                Err(error) if Instant::now() >= deadline => {
                    let kind = error.kind();
                    assert!(std::thread::panicking(), "scratch cleanup failed: {kind}");
                    eprintln!("scratch cleanup failed during unwind: {kind}");
                    return;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}
