//! Test scratch folders: one fresh folder per call, unique within this process by a counter and
//! across processes by the pid, so tests running in parallel never share one.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// Creates and returns a new empty folder under the temp folder; panics if it already exists.
pub(crate) fn dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "{}-{label}-{}-{}",
        kumochi_protocol::constants::OBJECT_PREFIX,
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&path).expect("a fresh scratch folder");
    path
}
