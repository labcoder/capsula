//! The tail's sequence lock: a read never returns a snapshot a write overlapped.

use std::sync::atomic::Ordering;

use super::{FAULT, Snapshot, Tail};

fn snapshot(n: u64) -> Snapshot {
    Snapshot {
        exposed: n,
        played: n - 1,
        flight: n + 1,
        quiet: 1,
        flags: FAULT,
    }
}

#[test]
fn a_read_refuses_a_write_in_progress_and_one_that_came_under_it() {
    let tail = Tail::default();
    tail.write(&snapshot(1));
    assert_eq!(tail.read(), Some(snapshot(1)));
    let seq = tail.seq.load(Ordering::Relaxed);
    tail.seq.store(seq + 1, Ordering::Relaxed);
    assert_eq!(tail.read(), None, "an odd stamp: a write in progress");
    tail.seq.store(seq, Ordering::Relaxed);
    tail.write(&snapshot(2));
    assert_eq!(
        tail.confirm(seq, snapshot(1)),
        None,
        "the stamp changed under the read"
    );
    assert_eq!(
        tail.read(),
        Some(snapshot(2)),
        "a read again gets the new one"
    );
}

#[test]
fn a_write_across_the_stamp_s_wrap_reads_back() {
    let tail = Tail::default();
    tail.seq.store(u64::MAX - 1, Ordering::Relaxed);
    tail.write(&snapshot(3));
    assert_eq!(tail.seq.load(Ordering::Relaxed), 0, "wrapped");
    assert_eq!(tail.read(), Some(snapshot(3)));
}
