//! Helpers shared by the brain's tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use capsula_protocol::constants;

use crate::runtime::Brain;
use crate::runtime::fixture::Fixture;
use crate::store::Store;

/// A store file in its own temp folder, removed afterwards.
pub(crate) struct Scratch(PathBuf);

impl Scratch {
    pub(crate) fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "{}-store-{}-{name}-{n}",
            constants::OBJECT_PREFIX,
            std::process::id()
        ));
        // A folder left by an earlier aborted run would make the test start from old data.
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("old scratch folder removable");
        }
        Self(dir)
    }

    pub(crate) fn db(&self) -> PathBuf {
        self.0.join("store.db")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            eprintln!("scratch folder left behind: {error}");
        }
    }
}

/// A brain on a fresh store in `scratch`, playing `script`.
pub(crate) fn brain(scratch: &Scratch, script: &str) -> Brain {
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recover");
    let high_water = store.high_water().expect("high water");
    Brain::new(store, high_water, Fixture::parse(script).expect("script")).expect("brain")
}
