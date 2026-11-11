use kumochi_protocol::geometry::PhysicalRect;

use super::*;
use crate::screen::wire::CaptureId;

#[test]
fn growing_a_frame_scrubs_the_old_allocation_before_freeing_it() {
    let mut frame = RawFrame::default();
    frame.begin(2, 2).unwrap().fill(0xAB);
    let (_, capacity) = frame.allocation();
    let (audit, scrubbed) = std::sync::mpsc::channel();
    frame.audit(audit);
    frame.begin(32, 32).unwrap();
    let old = scrubbed
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("old storage scrubbed before growth");
    assert_eq!(old, vec![0; capacity]);
    assert!(frame.allocation().1 >= 32 * 32 * 4);
}

#[test]
fn scrubbing_zeroes_the_whole_allocation_spare_capacity_included() {
    let mut frame = RawFrame::default();
    frame.begin(8, 8).expect("in bounds").fill(0xAB);
    frame.begin(2, 2).expect("in bounds").fill(0xCD);
    let (_, capacity) = frame.allocation();
    assert!(
        capacity >= 8 * 8 * 4,
        "the smaller frame kept the larger allocation"
    );
    let scrubbed = frame.scrubbed();
    assert_eq!(scrubbed.len(), capacity);
    assert_eq!(scrubbed.iter().filter(|&&byte| byte != 0).count(), 0);
    assert_eq!(frame.allocation().1, 0);
}

#[test]
fn release_frees_a_committed_image_and_a_later_capture_starts_fresh() {
    let size = [4, 3];
    let mut frame = RawFrame::default();
    frame.begin(size[0], size[1]).expect("in bounds").fill(0xEF);
    let meta = FrameMeta {
        capture: CaptureId(5),
        display: PhysicalRect {
            origin: [0, 0],
            size,
        },
        dpi: 96,
        acquired_after_ms: 0,
    };
    assert_eq!(frame.commit(meta), Ok(()));
    frame.release();
    assert_eq!(frame.meta(), None);
    assert_eq!(frame.allocation().1, 0);
    assert_eq!(frame.begin(size[0], size[1]).expect("in bounds"), &[0; 48]);
}
