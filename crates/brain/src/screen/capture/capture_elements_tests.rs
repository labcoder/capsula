use std::cell::RefCell;
use std::sync::mpsc;

use super::*;
use crate::screen::elements::{Element, ElementId};
use crate::screen::{Availability, ElementTarget};
use kumochi_protocol::geometry::WindowRef;

#[test]
fn captured_shell_identity_reaches_the_threaded_exclusion_before_text_is_read() {
    use crate::screen::elements::{ElementPlatform, SourceError};
    use std::sync::atomic::{AtomicU32, Ordering};
    #[derive(Clone)]
    struct Configured(Arc<AtomicU32>);
    impl ElementPlatform for Configured {
        fn set_shell_pid(&mut self, pid: u32) {
            self.0.store(pid, Ordering::Relaxed);
        }
        fn excluded(&mut self, pid: u32) -> Result<bool, SourceError> {
            Ok(pid == self.0.load(Ordering::Relaxed))
        }
    }
    let mut rig = Rig::new([4, 3], 7);
    let platform = Configured(Arc::new(AtomicU32::new(0)));
    rig.screen.reader = Some(Ok(Box::new(
        crate::screen::reader::ThreadedReader::new(
            || {
                let mut source = crate::screen::elements::tests::FakeSource::flat(1);
                source.nodes[0].rect = rect([4, 3]);
                source.nodes[0].name = "connected_shell_name_sentinel".into();
                Ok(source)
            },
            platform,
            || true,
        )
        .unwrap(),
    )));
    let now = rig.start;
    if let Some(Ok(reader)) = &mut rig.screen.reader {
        reader.set_shell_pid(8);
    }
    assert_eq!(rig.take(1, [4, 3], 0), Ok(meta(1, [4, 3], 0)));
    let walked = elements(
        &mut rig.screen,
        CaptureId(10),
        ElementTarget::Foreground,
        &|| now,
    )
    .unwrap();
    assert_eq!(
        walked.tree.availability,
        Availability::Unavailable(crate::screen::Unavailable::Excluded)
    );
    assert_eq!(walked.tree.elements.len(), 0);
    assert_eq!(
        rig.screen.masks.lock().unwrap().completeness(),
        crate::screen::mask::Completeness::Unknown(crate::screen::Unavailable::Excluded)
    );
}

#[test]
fn ticket_expiry_binds_even_while_the_frame_is_younger_than_250ms() {
    for (elapsed, expected) in [
        (
            1_999,
            Ok(crate::screen::ElementTree {
                capture: CaptureId(10),
                window: PhysicalRect {
                    origin: [0; 2],
                    size: [0; 2],
                },
                elements: Vec::new(),
                cut: None,
                availability: Availability::Unavailable(crate::screen::Unavailable::NoWindow),
            }),
        ),
        (2_000, Err(Failure::Refused(Refusal::Expired))),
    ] {
        let mut rig = Rig::new([4, 3], 7);
        assert_eq!(rig.take(1, [4, 3], 1_900), Ok(meta(1, [4, 3], 1_900)));
        assert_eq!(
            elements(
                &mut rig.screen,
                CaptureId(10),
                ElementTarget::Foreground,
                &|| { rig.start + Duration::from_millis(elapsed) }
            )
            .map(|walked| walked.tree),
            expected
        );
        assert_eq!(
            elements(
                &mut rig.screen,
                CaptureId(10),
                ElementTarget::Foreground,
                &|| rig.start
            )
            .err(),
            Some(Failure::Refused(Refusal::Reused))
        );
    }
}

#[test]
fn missing_reader_and_frame_metadata_have_distinct_faults() {
    let mut rig = Rig::new([4, 3], 7);
    assert_eq!(rig.take(1, [4, 3], 0), Ok(meta(1, [4, 3], 0)));
    rig.screen.reader = None;
    assert_eq!(
        elements(
            &mut rig.screen,
            CaptureId(10),
            ElementTarget::Foreground,
            &|| rig.start
        )
        .err(),
        Some(Failure::Fault(-1))
    );
    rig.screen.reader = Some(crate::screen::elements::tests::open_reader(Box::new(
        || true,
    )));
    assert_eq!(rig.take(2, [4, 3], 0), Ok(meta(2, [4, 3], 0)));
    rig.screen.frame.invalidate();
    assert_eq!(
        elements(
            &mut rig.screen,
            CaptureId(20),
            ElementTarget::Foreground,
            &|| rig.start
        )
        .err(),
        Some(Failure::Fault(-2))
    );
}

#[test]
fn reader_initialization_failure_preserves_capture_and_spends_the_faulted_walk() {
    let mut rig = Rig::new([4, 3], 7);
    let now = rig.start;
    rig.screen.reader = Some(Err(-7));
    assert_eq!(rig.take(1, [4, 3], 0), Ok(meta(1, [4, 3], 0)));
    assert_eq!(rig.screen.frame().meta(), Some(meta(1, [4, 3], 0)));
    assert_eq!(
        elements(
            &mut rig.screen,
            CaptureId(10),
            ElementTarget::Foreground,
            &|| now
        )
        .err(),
        Some(Failure::Fault(-7))
    );
    assert_eq!(
        rig.screen.masks.lock().unwrap().completeness(),
        crate::screen::mask::Completeness::Unknown(crate::screen::Unavailable::Busy)
    );
    assert_eq!(
        elements(
            &mut rig.screen,
            CaptureId(10),
            ElementTarget::Foreground,
            &|| now
        )
        .err(),
        Some(Failure::Refused(Refusal::Reused))
    );
}

/// Answers each walk at once: "A" for the window captured first, "B" for any other.
struct Probe {
    jobs: Rc<RefCell<Vec<WalkJob>>>,
    answer: Option<Result<Walked, Failure>>,
}

impl ElementReader for Probe {
    fn begin(&mut self, job: WalkJob) -> Result<(), Walked> {
        let name = if job.target
            == (ElementTarget::Window {
                window: WindowRef(7),
                pid: 8,
            }) {
            "A"
        } else {
            "B"
        };
        let tree = crate::screen::ElementTree {
            capture: job.capture,
            window: rect([4, 3]),
            elements: vec![Element {
                id: ElementId {
                    capture: job.capture,
                    index: 0,
                },
                parent: None,
                name: name.into(),
                name_cut: false,
                password: false,
                control_type: 0,
                screen: rect([4, 3]),
                in_window: PhysicalRect {
                    origin: [0; 2],
                    size: [4, 3],
                },
            }],
            cut: None,
            availability: Availability::Read,
        };
        self.jobs.borrow_mut().push(job);
        self.answer = Some(Ok(Walked {
            tree,
            retire: false,
        }));
        Ok(())
    }
    fn poll(&mut self) -> Option<Result<Walked, Failure>> {
        self.answer.take()
    }
}

#[test]
fn foreground_is_pinned_and_walk_budget_is_clamped_to_frame_and_ticket() {
    let mut rig = Rig::new([4, 3], 7);
    let jobs = Rc::new(RefCell::new(Vec::new()));
    rig.screen.reader = Some(Ok(Box::new(Probe {
        jobs: Rc::clone(&jobs),
        answer: None,
    })));
    assert_eq!(rig.take(1, [4, 3], 0), Ok(meta(1, [4, 3], 0)));
    rig.knobs.window.set(9);
    let walked = elements(
        &mut rig.screen,
        CaptureId(10),
        ElementTarget::Foreground,
        &|| rig.start + Duration::from_millis(200),
    )
    .unwrap();
    assert_eq!(walked.tree.elements[0].name, "A");
    assert_eq!(
        jobs.borrow()[0].until,
        rig.start + Duration::from_millis(250)
    );
    assert_eq!(
        format!(
            "{:?}",
            TopWindow {
                window: WindowRef(7),
                pid: 8,
                rect: rect([4, 3])
            }
        ),
        "TopWindow(..)"
    );
    assert_eq!(rig.take(2, [4, 3], 1_900), Ok(meta(2, [4, 3], 1_900)));
    let walked = elements(
        &mut rig.screen,
        CaptureId(20),
        ElementTarget::Foreground,
        &|| rig.start + Duration::from_millis(1_950),
    )
    .unwrap();
    assert_eq!(walked.tree.elements[0].name, "B");
    assert_eq!(
        jobs.borrow()[1].until,
        rig.start + Duration::from_millis(2_000)
    );
    assert_eq!(rig.take(3, [4, 3], 0), Ok(meta(3, [4, 3], 0)));
    let walked = elements(
        &mut rig.screen,
        CaptureId(30),
        ElementTarget::Foreground,
        &|| rig.start,
    )
    .unwrap();
    assert_eq!(walked.tree.elements.len(), 1);
    assert_eq!(
        jobs.borrow()[2].until,
        rig.start + Duration::from_millis(120)
    );
}

#[test]
fn new_capture_detaches_masks_from_a_late_walk_even_when_refused() {
    let mut rig = Rig::new([4, 3], 7);
    assert_eq!(rig.take(1, [4, 3], 0), Ok(meta(1, [4, 3], 0)));
    let old = Arc::clone(&rig.screen.masks);
    let late = Arc::clone(&old);
    let (release, wait) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        wait.recv().unwrap();
        late.lock().unwrap().push(PhysicalRect {
            origin: [-2559, 1],
            size: [1, 1],
        });
    });
    assert_eq!(rig.take(2, [4, 3], 0), Ok(meta(2, [4, 3], 0)));
    release.send(()).unwrap();
    thread.join().unwrap();
    assert_eq!(rig.screen.masks.lock().unwrap().rects(), []);
    assert_eq!(rig.screen.masks.lock().unwrap().generation(), 0);
    assert_eq!(
        old.lock().unwrap().rects(),
        [PhysicalRect {
            origin: [-2559, 1],
            size: [1, 1]
        }]
    );
    rig.screen.masks.lock().unwrap().push(rect([4, 3]));
    assert_eq!(rig.take(2, [4, 3], 0), Err(Refusal::Reused.into()));
    assert_eq!(rig.screen.masks.lock().unwrap().rects(), []);
    assert_eq!(rig.screen.masks.lock().unwrap().generation(), 0);
    assert_eq!(
        elements(
            &mut rig.screen,
            CaptureId(20),
            ElementTarget::Foreground,
            &|| rig.start
        )
        .err(),
        Some(Refusal::Stale.into())
    );
}
