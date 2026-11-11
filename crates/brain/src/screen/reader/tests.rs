use super::*;
use crate::screen::ElementTarget;
use crate::screen::elements::{Availability, NodeProps, SourceError};
use crate::screen::mask::{Completeness, MaskSet};
use kumochi_protocol::geometry::{PhysicalRect, WindowRef};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

const RECT: PhysicalRect = PhysicalRect {
    origin: [-10, 20],
    size: [100, 50],
};

#[test]
fn the_instrumented_clock_observes_the_walks_deadline_check() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let checks = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&checks);
    let mut reader = ThreadedReader::with_clock(
        || {
            Ok(Source {
                read: |_| props("clock"),
                count: 0,
                _local: Rc::new(()),
            })
        },
        Platform,
        move || {
            observed.fetch_add(1, Ordering::Relaxed);
            Instant::now()
        },
        || true,
    )
    .unwrap();
    let walked = reader
        .walk(job(31, Instant::now() + Duration::from_secs(1), masks(31)))
        .unwrap();
    assert_eq!(walked.tree.availability, Availability::Read);
    assert_eq!(walked.tree.elements.len(), 1);
    assert!(
        checks.load(Ordering::Relaxed) >= 1,
        "the walk reads the injected clock"
    );
}

#[derive(Clone)]
struct Platform;
impl ElementPlatform for Platform {
    fn foreground(&mut self) -> Option<TopWindow> {
        Some(TopWindow {
            window: WindowRef(17),
            pid: 23,
            rect: RECT,
        })
    }
    fn excluded(&mut self, _pid: u32) -> Result<bool, SourceError> {
        Ok(false)
    }
}

struct Source<F> {
    read: F,
    count: u32,
    _local: Rc<()>,
}
impl<F: FnMut(u32) -> NodeProps> ElementSource for Source<F> {
    type Node = ();
    fn root(&mut self, _target: ElementTarget) -> Result<((), PhysicalRect, u32), SourceError> {
        Ok(((), RECT, 23))
    }
    fn read(&mut self, _node: &()) -> Result<NodeProps, SourceError> {
        self.count += 1;
        Ok((self.read)(self.count))
    }
    fn first_child(&mut self, _node: &()) -> Result<Option<()>, SourceError> {
        Ok(None)
    }
    fn next_sibling(&mut self, _node: &()) -> Result<Option<()>, SourceError> {
        Ok(None)
    }
}

fn reader(read: impl FnMut(u32) -> NodeProps + Send + 'static) -> ThreadedReader<Platform> {
    waking(read, || true)
}
fn waking(
    read: impl FnMut(u32) -> NodeProps + Send + 'static,
    wake: impl Fn() -> bool + Send + 'static,
) -> ThreadedReader<Platform> {
    let mut read = Some(read);
    ThreadedReader::new(
        move || {
            Ok(Source {
                read: read.take().unwrap(),
                count: 0,
                _local: Rc::new(()),
            })
        },
        Platform,
        wake,
    )
    .unwrap()
}
fn props(name: &str) -> NodeProps {
    NodeProps {
        name: name.into(),
        password: false,
        control_type: 50000,
        rect: RECT,
    }
}
fn job(capture: u64, until: Instant, masks: MaskSink) -> WalkJob {
    WalkJob {
        capture: CaptureId(capture),
        target: ElementTarget::Foreground,
        display: RECT,
        until,
        masks,
    }
}
fn masks(capture: u64) -> MaskSink {
    Arc::new(Mutex::new(MaskSet::new(CaptureId(capture))))
}
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "fixture compares the reply's single retire flag"
)]
fn assert_busy(walked: &Walked, capture: u64, retire: bool) {
    assert_eq!(
        walked.tree,
        ElementTree {
            capture: CaptureId(capture),
            window: PhysicalRect {
                origin: [0; 2],
                size: [0; 2]
            },
            elements: Vec::new(),
            cut: None,
            availability: Availability::Unavailable(Unavailable::Busy)
        }
    );
    assert_eq!(walked.retire, retire);
}

#[test]
fn failed_source_initialization_is_a_fault_and_the_next_capture_retries() {
    let mut attempts = 0;
    let mut reader = ThreadedReader::new(
        move || {
            attempts += 1;
            if attempts == 1 {
                return Err(-7);
            }
            Ok(Source {
                read: |_| props("retry"),
                count: 0,
                _local: Rc::new(()),
            })
        },
        Platform,
        || true,
    )
    .unwrap();
    let sink = masks(20);
    assert_eq!(
        reader
            .walk(job(
                20,
                Instant::now() + Duration::from_secs(1),
                Arc::clone(&sink)
            ))
            .err(),
        Some(Failure::Fault(-7))
    );
    assert_eq!(
        sink.lock().unwrap().completeness(),
        Completeness::Unknown(Unavailable::Busy)
    );
    let next_masks = masks(21);
    let walked = reader
        .walk(job(
            21,
            Instant::now() + Duration::from_secs(1),
            Arc::clone(&next_masks),
        ))
        .unwrap();
    assert!(!walked.retire);
    assert_eq!(walked.tree.capture, CaptureId(21));
    assert_eq!(walked.tree.availability, Availability::Read);
    assert_eq!(walked.tree.elements.len(), 1);
    assert_eq!(walked.tree.elements[0].name, "retry");
    assert_eq!(
        next_masks.lock().unwrap().completeness(),
        Completeness::Complete
    );
}

#[test]
fn timeout_then_still_busy_retires_and_drop_does_not_join() {
    let old_masks = masks(1);
    let source_masks = Arc::clone(&old_masks);
    let (release, released) = mpsc::channel();
    let (finished, finish) = mpsc::channel();
    let mut reader = reader(move |_| {
        source_masks.lock().unwrap().push(RECT);
        let result = released.recv_timeout(Duration::from_secs(3));
        finished.send(result).unwrap();
        props("late")
    });
    let start = Instant::now();
    let first = reader.walk(job(
        1,
        start + Duration::from_millis(50),
        Arc::clone(&old_masks),
    ));
    let first_elapsed = start.elapsed();
    let new_masks = masks(2);
    let second = reader.walk(job(
        2,
        Instant::now() + Duration::from_secs(1),
        Arc::clone(&new_masks),
    ));
    let dropping = Instant::now();
    drop(reader);
    let drop_elapsed = dropping.elapsed();
    release.send(()).unwrap();
    assert_eq!(finish.recv_timeout(Duration::from_secs(1)).unwrap(), Ok(()));
    assert_busy(&first.unwrap(), 1, false);
    assert_busy(&second.unwrap(), 2, true);
    assert_eq!(old_masks.lock().unwrap().rects(), &[RECT]);
    assert_eq!(new_masks.lock().unwrap().rects(), &[]);
    assert_eq!(
        new_masks.lock().unwrap().completeness(),
        Completeness::Unknown(Unavailable::Busy)
    );
    assert!(first_elapsed >= Duration::from_millis(200));
    assert!(first_elapsed < Duration::from_secs(1));
    assert!(drop_elapsed < Duration::from_millis(50));
}

#[test]
fn late_reply_is_drained_and_next_capture_gets_its_own_tree() {
    let until = Instant::now() + Duration::from_millis(100);
    let (release, released) = mpsc::channel();
    let mut reader = reader(move |count| {
        if count == 1 {
            released.recv_timeout(Duration::from_secs(3)).unwrap();
        }
        props(if count == 1 { "obsolete" } else { "fresh" })
    });
    let (forward, forwarded) = mpsc::sync_channel(1);
    let original = std::mem::replace(&mut reader.replies, forwarded);
    let (queued, notification) = mpsc::channel();
    let relay = std::thread::spawn(move || {
        while let Ok(reply) = original.recv() {
            let capture = reply.0;
            if forward.send(reply).is_err() {
                break;
            }
            if queued.send(capture).is_err() {
                break;
            }
        }
    });
    let old_masks = masks(1);
    let first = reader.walk(job(1, until, Arc::clone(&old_masks)));
    release.send(()).unwrap();
    assert_eq!(
        notification.recv_timeout(Duration::from_secs(1)).unwrap(),
        CaptureId(1)
    );
    assert_eq!(
        old_masks.lock().unwrap().completeness(),
        Completeness::Unknown(Unavailable::Busy)
    );
    let fresh_masks = masks(2);
    let second = reader.walk(job(
        2,
        Instant::now() + Duration::from_secs(1),
        Arc::clone(&fresh_masks),
    ));
    drop(reader);
    relay.join().unwrap();
    assert_busy(&first.unwrap(), 1, false);
    let second = second.unwrap();
    assert!(!second.retire);
    assert_eq!(second.tree.capture, CaptureId(2));
    assert_eq!(second.tree.availability, Availability::Read);
    assert_eq!(second.tree.cut, None);
    assert_eq!(second.tree.elements.len(), 1);
    assert_eq!(second.tree.elements[0].name, "fresh");
    assert_eq!(second.tree.elements[0].id.capture, CaptureId(2));
    assert_eq!(
        fresh_masks.lock().unwrap().completeness(),
        Completeness::Complete
    );
}

#[test]
fn cached_read_finishing_100ms_after_budget_is_accepted_within_slack() {
    let until = Instant::now() + Duration::from_millis(500);
    let (entered, entering) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let mut reader = reader(move |_| {
        entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        props("complete")
    });
    let trigger = std::thread::spawn(move || {
        entering.recv_timeout(Duration::from_secs(1)).unwrap();
        std::thread::sleep(
            (until + Duration::from_millis(100)).saturating_duration_since(Instant::now()),
        );
        release.send(()).unwrap();
    });
    let sink = masks(3);
    let result = reader.walk(job(3, until, Arc::clone(&sink)));
    trigger.join().unwrap();
    let walked = result.unwrap();
    assert!(!walked.retire);
    assert_eq!(walked.tree.capture, CaptureId(3));
    assert_eq!(walked.tree.availability, Availability::Read);
    assert_eq!(walked.tree.cut, None);
    assert_eq!(walked.tree.elements.len(), 1);
    assert_eq!(walked.tree.elements[0].name, "complete");
    assert_eq!(sink.lock().unwrap().completeness(), Completeness::Complete);
}

#[test]
fn poisoned_masks_retire_and_a_clean_sink_cannot_revive_the_reader() {
    let poisoned = masks(4);
    let to_poison = Arc::clone(&poisoned);
    assert!(
        std::thread::spawn(move || {
            let _guard = to_poison.lock().unwrap();
            panic!("fixture poisoning");
        })
        .join()
        .is_err()
    );
    let mut reader = reader(|_| props("unread"));
    assert_busy(
        &reader
            .walk(job(4, Instant::now(), Arc::clone(&poisoned)))
            .unwrap(),
        4,
        true,
    );
    assert_busy(
        &reader.walk(job(5, Instant::now(), masks(5))).unwrap(),
        5,
        true,
    );
    assert_eq!(
        poisoned.lock().unwrap_err().into_inner().completeness(),
        Completeness::Unknown(Unavailable::Busy)
    );
}

#[test]
fn a_panicked_source_retires_instead_of_looking_like_an_empty_tree() {
    let mut reader = reader(|_| panic!("fixture provider panic"));
    let sink = masks(6);
    assert_busy(
        &reader
            .walk(job(
                6,
                Instant::now() + Duration::from_secs(1),
                Arc::clone(&sink),
            ))
            .unwrap(),
        6,
        true,
    );
    assert_busy(
        &reader.walk(job(7, Instant::now(), masks(7))).unwrap(),
        7,
        true,
    );
    assert_eq!(
        sink.lock().unwrap().completeness(),
        Completeness::Unknown(Unavailable::Busy)
    );
}

#[test]
fn masks_poisoned_during_a_successful_read_still_retire() {
    let sink = masks(12);
    let source_sink = Arc::clone(&sink);
    let mut reader = reader(move |_| {
        let to_poison = Arc::clone(&source_sink);
        assert!(
            std::thread::spawn(move || {
                let _guard = to_poison.lock().unwrap();
                panic!("fixture poisoning during read");
            })
            .join()
            .is_err()
        );
        NodeProps {
            password: true,
            ..props("must not publish")
        }
    });
    assert_busy(
        &reader
            .walk(job(
                12,
                Instant::now() + Duration::from_secs(1),
                Arc::clone(&sink),
            ))
            .unwrap(),
        12,
        true,
    );
    assert_eq!(
        sink.lock().unwrap_err().into_inner().completeness(),
        Completeness::Unknown(Unavailable::Busy)
    );
}

#[test]
fn a_disconnected_job_receiver_retires_without_waiting() {
    let (jobs, requests) = mpsc::sync_channel(1);
    let (_answers, replies) = mpsc::sync_channel(1);
    drop(requests);
    let mut reader = ThreadedReader {
        platform: Platform,
        jobs,
        replies,
        pending: None,
        retired: false,
    };
    assert_busy(
        &reader
            .walk(job(13, Instant::now() + Duration::from_secs(1), masks(13)))
            .unwrap(),
        13,
        true,
    );
}

#[test]
fn full_job_queue_never_blocks_and_a_second_attempt_retires() {
    let (jobs, requests) = mpsc::sync_channel(1);
    jobs.try_send(job(90, Instant::now(), masks(90)))
        .ok()
        .unwrap();
    let (_answers, replies) = mpsc::sync_channel(1);
    let mut reader = ThreadedReader {
        platform: Platform,
        jobs,
        replies,
        pending: None,
        retired: false,
    };
    let (release, released) = mpsc::channel();
    let consumer = std::thread::spawn(move || {
        let wake = released.recv_timeout(Duration::from_secs(3));
        let queued = requests.recv_timeout(Duration::from_secs(1)).unwrap();
        (wake, queued.capture)
    });
    let start = Instant::now();
    let sink = masks(8);
    let first = reader.walk(job(8, start + Duration::from_secs(1), Arc::clone(&sink)));
    let elapsed = start.elapsed();
    let second = reader.walk(job(9, Instant::now(), masks(9)));
    let sent = release.send(());
    let (wake, queued) = consumer.join().unwrap();
    assert_eq!(sent, Ok(()));
    assert_eq!(wake, Ok(()));
    assert_eq!(queued, CaptureId(90));
    assert_busy(&first.unwrap(), 8, false);
    assert_busy(&second.unwrap(), 9, true);
    assert_eq!(
        sink.lock().unwrap().completeness(),
        Completeness::Unknown(Unavailable::Busy)
    );
    assert!(elapsed < Duration::from_millis(500));
}

#[test]
fn a_wrong_capture_tag_in_the_drain_retires_without_starting_work() {
    let (jobs, requests) = mpsc::sync_channel(1);
    let (answers, replies) = mpsc::sync_channel(1);
    answers
        .try_send((
            CaptureId(99),
            Ok(ElementTree::unavailable(
                CaptureId(99),
                Unavailable::NoWindow,
            )),
        ))
        .ok()
        .unwrap();
    let mut reader = ThreadedReader {
        platform: Platform,
        jobs,
        replies,
        pending: Some((CaptureId(10), masks(10))),
        retired: false,
    };
    assert_busy(
        &reader.walk(job(11, Instant::now(), masks(11))).unwrap(),
        11,
        true,
    );
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
}

/// A read that reports entering, then waits for its release.
fn gate() -> (
    impl FnMut(u32) -> NodeProps + Send + 'static,
    mpsc::Receiver<()>,
    mpsc::Sender<()>,
) {
    let (entered, entering) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let read = move |_| {
        entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(5)).ok();
        props("gated")
    };
    (read, entering, release)
}

/// A wake that reports each call. The thread wakes only after the completion is in its slot, so
/// one report means one `poll` answers.
fn told() -> (impl Fn() -> bool + Send + 'static, mpsc::Receiver<()>) {
    let (tell, told) = mpsc::channel();
    (move || tell.send(()).is_ok(), told)
}

fn later() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

#[test]
fn a_gone_listener_disposes_the_source_and_cannot_accept_another_walk() {
    struct Gone(mpsc::Sender<()>);
    impl Drop for Gone {
        fn drop(&mut self) {
            self.0.send(()).expect("test listens");
        }
    }
    let (dropped, drops) = mpsc::channel();
    let guard = Gone(dropped);
    let mut reader = waking(
        move |_| {
            let _keep = &guard;
            props("listener_gone")
        },
        || false,
    );
    assert!(reader.begin(job(50, later(), masks(50))).is_ok());
    drops
        .recv_timeout(Duration::from_secs(5))
        .expect("closed listener ends the source");
    let completed = reader.poll().expect("the slot is still taken").unwrap();
    assert_busy(&completed, 50, true);
    let Err(refused) = reader.begin(job(51, later(), masks(51))) else {
        panic!("a walk was admitted without a listener");
    };
    assert_busy(&refused, 51, true);
}

#[test]
fn a_pending_walk_never_blocks_poll_and_a_dropped_wake_keeps_its_completion() {
    let (listener, woken) = mpsc::sync_channel(1);
    listener.try_send(()).unwrap();
    let (attempted, attempts) = mpsc::channel();
    let (read, entering, release) = gate();
    let mut reader = waking(read, move || {
        let sent = listener.try_send(());
        let open = !matches!(sent, Err(mpsc::TrySendError::Disconnected(())));
        attempted.send(sent.is_ok()).is_ok() && open
    });
    assert!(matches!(reader.begin(job(40, later(), masks(40))), Ok(())));
    entering.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(reader.poll().is_none(), "the walk is still running");
    release.send(()).unwrap();
    assert_eq!(
        attempts.recv_timeout(Duration::from_secs(1)),
        Ok(false),
        "the full listener dropped the wake"
    );
    let first = reader
        .poll()
        .expect("the completion outlived its wake")
        .unwrap();
    assert_eq!(first.tree.capture, CaptureId(40));
    assert_eq!(first.tree.elements[0].name, "gated");
    assert_eq!(woken.try_recv(), Ok(()));
    assert_eq!(woken.try_recv(), Err(TryRecvError::Empty));
    release.send(()).unwrap();
    assert!(matches!(reader.begin(job(41, later(), masks(41))), Ok(())));
    assert_eq!(
        attempts.recv_timeout(Duration::from_secs(1)),
        Ok(true),
        "a free listener gets the wake"
    );
    let second = reader.poll().expect("landed before its wake").unwrap();
    assert_eq!(second.tree.capture, CaptureId(41));
    assert!(!second.retire);
    assert_eq!(woken.try_recv(), Ok(()));
}

#[test]
fn begin_while_a_walk_is_pending_retires_without_queueing_a_second() {
    let (read, entering, release) = gate();
    let mut reader = waking(read, || true);
    assert!(matches!(reader.begin(job(42, later(), masks(42))), Ok(())));
    entering.recv_timeout(Duration::from_secs(1)).unwrap();
    let sink = masks(43);
    let Err(refused) = reader.begin(job(43, later(), Arc::clone(&sink))) else {
        panic!("a second job was queued");
    };
    assert_busy(&refused, 43, true);
    assert_eq!(
        sink.lock().unwrap().completeness(),
        Completeness::Unknown(Unavailable::Busy)
    );
    release.send(()).unwrap();
}

#[test]
fn begin_after_an_untaken_completion_retires_and_keeps_that_completion() {
    let (wake, landed) = told();
    let mut reader = waking(|_| props("untaken"), wake);
    assert!(matches!(reader.begin(job(44, later(), masks(44))), Ok(())));
    landed.recv_timeout(Duration::from_secs(1)).unwrap();
    let sink = masks(45);
    let Err(refused) = reader.begin(job(45, later(), Arc::clone(&sink))) else {
        panic!("begin dropped the untaken completion");
    };
    assert_busy(&refused, 45, true);
    assert_eq!(
        sink.lock().unwrap().completeness(),
        Completeness::Unknown(Unavailable::Busy)
    );
    let kept = reader.poll().expect("still in its slot").unwrap();
    assert_eq!(kept.tree.capture, CaptureId(44));
    assert_eq!(kept.tree.elements[0].name, "untaken");
    assert!(
        kept.retire,
        "taking a completion cannot revive a retired reader"
    );
}

#[test]
fn a_blocked_walk_does_not_delay_the_next_captures_foreground() {
    let (read, entering, release) = gate();
    let (wake, landed) = told();
    let mut reader = waking(read, wake);
    assert!(matches!(reader.begin(job(46, later(), masks(46))), Ok(())));
    entering.recv_timeout(Duration::from_secs(1)).unwrap();
    let top = reader.foreground().expect("the platform's foreground");
    assert_eq!((top.window, top.pid, top.rect), (WindowRef(17), 23, RECT));
    assert!(reader.poll().is_none(), "the walk is still blocked");
    release.send(()).unwrap();
    landed.recv_timeout(Duration::from_secs(1)).unwrap();
    let walked = reader.poll().expect("landed before its wake").unwrap();
    assert_eq!(walked.tree.capture, CaptureId(46));
}
