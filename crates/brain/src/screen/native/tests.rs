use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use super::*;
use crate::screen::actor::{Key, Owner};
use crate::screen::capture::Target;
use crate::screen::elements::TopWindow;
use crate::screen::wire::{CaptureId, Refusal};
use kumochi_protocol::geometry::{PhysicalPoint, PhysicalRect};

const PATIENCE: Duration = Duration::from_secs(5);

struct Local {
    owner: std::thread::ThreadId,
    dropped: mpsc::Sender<std::thread::ThreadId>,
    _not_send: Rc<()>,
}

impl Capturer for Local {
    fn pointer_window(&mut self, _: PhysicalPoint, _: u32) -> Result<Target, Refusal> {
        Ok(Target {
            top: TopWindow {
                window: kumochi_protocol::geometry::WindowRef(7),
                pid: 8,
                rect: PhysicalRect {
                    origin: [0, 0],
                    size: [2, 2],
                },
            },
            instance: 9,
            dpi: 96,
        })
    }
    fn capture_window(
        &mut self,
        target: &Target,
        _: Instant,
        frame: &mut RawFrame,
    ) -> Result<Acquired, Failure> {
        if self.owner != std::thread::current().id() {
            return Err(Failure::Fault(-99));
        }
        let [width, height] = target.top.rect.size;
        frame.begin(width, height)?.fill(91);
        Ok(Acquired {
            arrived: Instant::now(),
            dpi: 96,
        })
    }
}

impl Drop for Local {
    fn drop(&mut self) {
        self.dropped
            .send(std::thread::current().id())
            .expect("test listens");
    }
}

fn job() -> Job {
    Job {
        key: Key {
            owner: Owner {
                epoch: 4,
                gesture: 2,
            },
            capture: CaptureId(7),
        },
        frame: RawFrame::default(),
        point: PhysicalPoint([1, 1]),
        aim: Aim::Window { shell: 0 },
        until: Instant::now() + PATIENCE,
    }
}

#[test]
fn the_non_send_capturer_is_constructed_used_and_dropped_on_its_one_thread() {
    let (created, creating) = mpsc::channel();
    let (dropped, dropping) = mpsc::channel();
    let (wake, wakes) = mpsc::channel();
    let mut native = CaptureThread::open(
        move || {
            let owner = std::thread::current().id();
            created.send(owner).expect("test listens");
            Ok(Box::new(Local {
                owner,
                dropped: dropped.clone(),
                _not_send: Rc::new(()),
            }))
        },
        Box::new(move || wake.send(()).is_ok()),
    )
    .expect("capture thread");
    assert!(native.begin(job()).is_none());
    wakes.recv_timeout(PATIENCE).unwrap();
    assert!(
        native.begin(job()).is_some(),
        "untaken completion reserves the producer"
    );
    let (key, frame, acquired) = native.poll().unwrap().unwrap();
    assert_eq!(key.capture, CaptureId(7));
    assert_eq!(frame.allocation().1, 16);
    assert_eq!(acquired.unwrap().0.dpi, 96);
    let owner = creating.recv_timeout(PATIENCE).unwrap();
    assert_ne!(owner, std::thread::current().id());
    drop(native);
    assert_eq!(dropping.recv_timeout(PATIENCE).unwrap(), owner);
}

#[test]
fn a_failed_open_answers_fault_and_the_next_job_opens_again() {
    let (dropped, dropping) = mpsc::channel();
    let (wake, wakes) = mpsc::channel();
    let mut opens = 0;
    let mut native = CaptureThread::open(
        move || {
            opens += 1;
            if opens == 1 {
                return Err(Failure::Fault(-5));
            }
            Ok(Box::new(Local {
                owner: std::thread::current().id(),
                dropped: dropped.clone(),
                _not_send: Rc::new(()),
            }))
        },
        Box::new(move || wake.send(()).is_ok()),
    )
    .unwrap();
    assert!(native.begin(job()).is_none());
    wakes.recv_timeout(PATIENCE).unwrap();
    let (_, _, failed) = native.poll().unwrap().unwrap();
    assert!(matches!(failed, Err(Failure::Fault(-5))), "{failed:?}");
    assert!(native.begin(job()).is_none());
    wakes.recv_timeout(PATIENCE).unwrap();
    let (_, _, acquired) = native.poll().unwrap().unwrap();
    assert_eq!(acquired.unwrap().0.dpi, 96);
    drop(native);
    dropping.recv_timeout(PATIENCE).unwrap();
}

#[test]
fn a_full_wake_does_not_block_the_reserved_completion_and_a_gone_listener_ends_the_thread() {
    let (wake, wakes) = mpsc::sync_channel(1);
    wake.try_send(()).unwrap();
    let (attempted, attempts) = mpsc::channel();
    let (dropped, dropping) = mpsc::channel();
    let mut native = CaptureThread::open(
        move || {
            Ok(Box::new(Local {
                owner: std::thread::current().id(),
                dropped: dropped.clone(),
                _not_send: Rc::new(()),
            }))
        },
        Box::new(move || {
            let result = wake.try_send(());
            let open = !matches!(result, Err(mpsc::TrySendError::Disconnected(())));
            attempted.send(result.is_ok()).is_ok() && open
        }),
    )
    .unwrap();
    assert!(native.begin(job()).is_none());
    assert_eq!(attempts.recv_timeout(PATIENCE), Ok(false));
    assert_eq!(native.poll().unwrap().unwrap().0.capture, CaptureId(7));
    drop(wakes);
    assert!(native.begin(job()).is_none());
    assert_eq!(attempts.recv_timeout(PATIENCE), Ok(false));
    dropping.recv_timeout(PATIENCE).unwrap();
    assert_eq!(native.poll().unwrap().unwrap().0.capture, CaptureId(7));
    assert!(
        native.begin(job()).is_some(),
        "the closed listener ended the producer"
    );
}
