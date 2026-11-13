//! Test scratch folders: one fresh folder per call, unique within this process by a counter and
//! across processes by the pid, so tests running in parallel never share one.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// Creates and returns a new empty folder under the temp folder, replacing one of the same name.
pub(crate) fn dir(label: &str) -> PathBuf {
    numbered(label, NEXT.fetch_add(1, Ordering::Relaxed))
}

fn numbered(label: &str, number: u64) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "{}-{label}-{}-{number}",
        kumochi_protocol::constants::OBJECT_PREFIX,
        std::process::id(),
    ));
    // The name carries our pid, so a folder already there was left by a dead process with that pid.
    if let Err(error) = std::fs::remove_dir_all(&path) {
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::NotFound,
            "a stale scratch folder is removed"
        );
    }
    std::fs::create_dir(&path).expect("a fresh scratch folder");
    path
}

#[cfg(test)]
mod tests {
    use super::numbered;

    #[test]
    fn a_folder_left_under_the_same_name_is_replaced() {
        let stale = numbered("scratch-stale", u64::MAX);
        std::fs::write(stale.join("left.txt"), b"old").expect("a planted file");

        let fresh = numbered("scratch-stale", u64::MAX);

        assert_eq!(fresh, stale);
        assert_eq!(
            std::fs::read_dir(&fresh).expect("the fresh folder").count(),
            0
        );
    }
}
