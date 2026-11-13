use std::io::{self, Cursor, Read, Write};
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use super::*;
use crate::screen::capture::{Acquired, Target};
use crate::screen::elements::{
    ElementPlatform, ElementReader, ElementSource, NodeProps, SourceError, TopWindow,
};
use crate::screen::elements::{WalkJob, Walked};
use crate::screen::frame::RawFrame;
use crate::screen::mask::MaskSink;
use crate::screen::reader::ThreadedReader;
use crate::screen::wire::Exclusion;
use crate::supervise::write_message;
use kumochi_protocol::geometry::{DisplayRef, PhysicalPoint, PhysicalRect, WindowRef};

mod replacement;

const PATIENCE: Duration = Duration::from_secs(5);
const RECT: PhysicalRect = PhysicalRect {
    origin: [0, 0],
    size: [4, 3],
};

fn key(id: u64) -> Key {
    Key {
        owner: Owner {
            epoch: 1,
            gesture: 7,
        },
        capture: CaptureId(id),
    }
}

fn context(id: u64) -> Request {
    context_for(id, key(id).owner)
}

fn context_for(id: u64, owner: Owner) -> Request {
    Request::Context {
        key: Key {
            owner,
            capture: CaptureId(id),
        },
        ticket: TicketWire::new(CaptureId(id), id, [91; 16], 30_000),
        point: PhysicalPoint([1, 1]),
    }
}

/// The fake's window under `point` reads as changed by the time it's sealed.
const CHANGES: PhysicalPoint = PhysicalPoint([1, 2]);

fn unchanged(target: &Target) -> bool {
    target.instance == 9
}

struct Input {
    receive: mpsc::Receiver<Vec<u8>>,
    current: Cursor<Vec<u8>>,
}

impl Read for Input {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let n = self.current.read(buffer)?;
            if n != 0 {
                return Ok(n);
            }
            let Ok(bytes) = self.receive.recv() else {
                return Ok(0);
            };
            self.current = Cursor::new(bytes);
        }
    }
}

struct Output {
    send: mpsc::Sender<(u64, Reply)>,
    bytes: Vec<u8>,
}

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        let reply = read_request::<Reply>(&mut self.bytes.as_slice(), Kind::MAX_REPLY)
            .expect("whole reply")
            .expect("one reply");
        self.send
            .send(reply)
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        self.bytes.clear();
        Ok(())
    }
}

struct Gate {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}
impl Gate {
    fn wait(self) {
        self.entered.send(()).expect("test listens");
        self.release.recv_timeout(PATIENCE).expect("test releases");
    }
}
fn gate() -> (Gate, mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered, entering) = mpsc::channel();
    let (release, released) = mpsc::channel();
    (
        Gate {
            entered,
            release: released,
        },
        entering,
        release,
    )
}

struct Fake {
    clock: Arc<Mutex<Instant>>,
    foreground: Option<Gate>,
    capture: Option<Gate>,
    audit: mpsc::Sender<Vec<u8>>,
    /// Taken by the first capture that reaches it.
    completion: Option<Failure>,
    sampled: mpsc::Sender<thread::ThreadId>,
    _local: Rc<()>,
}
impl Capturer for Fake {
    /// Resolves the walk's window; a point left of the display has none.
    fn pointer_window(&mut self, point: PhysicalPoint, _: u32) -> Result<Target, Refusal> {
        if let Some(gate) = self.foreground.take() {
            gate.wait();
        }
        self.sampled
            .send(thread::current().id())
            .expect("test listens");
        if point.0[0] < 0 {
            return Err(Refusal::NoWindow);
        }
        Ok(Target {
            top: TopWindow {
                window: WindowRef(7),
                pid: 8,
                rect: RECT,
            },
            instance: if point == CHANGES { 10 } else { 9 },
            dpi: 96,
        })
    }
    fn capture_window(
        &mut self,
        _: &Target,
        until: Instant,
        frame: &mut RawFrame,
    ) -> Result<Acquired, Failure> {
        self.capture_into(&DisplayRef(RECT), Exclusion::KumochiWindows, until, frame)
    }
    fn capture_into(
        &mut self,
        _: &DisplayRef,
        _: Exclusion,
        _: Instant,
        frame: &mut RawFrame,
    ) -> Result<Acquired, Failure> {
        frame.audit(self.audit.clone());
        frame.begin(4, 3)?.fill(91);
        if let Some(gate) = self.capture.take() {
            gate.wait();
        }
        if let Some(failure) = self.completion.take() {
            return Err(failure);
        }
        Ok(Acquired {
            arrived: *self.clock.lock().unwrap(),
            dpi: 96,
        })
    }
}

struct Source(Option<Gate>);
impl ElementSource for Source {
    type Node = ();
    fn root(&mut self, _: ElementTarget) -> Result<((), PhysicalRect, u32), SourceError> {
        Ok(((), RECT, 8))
    }
    fn read(&mut self, (): &()) -> Result<NodeProps, SourceError> {
        if let Some(gate) = self.0.take() {
            gate.wait();
        }
        Ok(NodeProps {
            name: "late_tree_content_sentinel".into(),
            password: false,
            control_type: 0,
            rect: RECT,
        })
    }
    fn first_child(&mut self, (): &()) -> Result<Option<()>, SourceError> {
        Ok(None)
    }
    fn next_sibling(&mut self, (): &()) -> Result<Option<()>, SourceError> {
        Ok(None)
    }
}
#[derive(Clone)]
struct Platform;
impl ElementPlatform for Platform {
    fn foreground(&mut self) -> Option<TopWindow> {
        None
    }
    fn excluded(&mut self, _: u32) -> Result<bool, SourceError> {
        Ok(false)
    }
}
struct Reader {
    inner: ThreadedReader<Platform>,
    masks: mpsc::Sender<MaskSink>,
}
impl ElementReader for Reader {
    fn foreground(&mut self) -> Option<TopWindow> {
        self.inner.foreground()
    }
    fn set_shell_pid(&mut self, pid: u32) {
        self.inner.set_shell_pid(pid);
    }
    fn begin(&mut self, job: WalkJob) -> Result<(), Walked> {
        self.masks
            .send(Arc::clone(&job.masks))
            .expect("test listens");
        self.inner.begin(job)
    }
    fn poll(&mut self) -> Option<Result<Walked, Failure>> {
        self.inner.poll()
    }
    fn walk(&mut self, job: WalkJob) -> Result<Walked, Failure> {
        self.inner.walk(job)
    }
}
struct Setup {
    gate: Option<Gate>,
    now: Instant,
    masks: mpsc::Sender<MaskSink>,
}
thread_local! {
    static SOURCE: std::cell::RefCell<Option<Setup>> = const { std::cell::RefCell::new(None) };
}
fn reader(wake: Wake) -> Result<Box<dyn ElementReader>, i32> {
    let Setup {
        mut gate,
        now,
        masks,
    } = SOURCE.with(|source| source.borrow_mut().take().unwrap());
    ThreadedReader::with_clock(move || Ok(Source(gate.take())), Platform, move || now, wake)
        .map(|inner| -> Box<dyn ElementReader> { Box::new(Reader { inner, masks }) })
}

struct Rig {
    requests: mpsc::Sender<Vec<u8>>,
    replies: mpsc::Receiver<(u64, Reply)>,
    scrubbed: mpsc::Receiver<Vec<u8>>,
    masks: mpsc::Receiver<MaskSink>,
    sampled: mpsc::Receiver<thread::ThreadId>,
    clock: Arc<Mutex<Instant>>,
    /// Capturers the worker opened.
    opened: Arc<std::sync::atomic::AtomicUsize>,
    done: thread::JoinHandle<Result<(), frame::FrameError>>,
}

#[derive(Clone, Copy)]
enum Clock {
    Manual,
    Realtime,
}

impl Rig {
    fn start(foreground: Option<Gate>, capture: Option<Gate>, walk: Option<Gate>) -> Self {
        Self::create(foreground, capture, walk, Clock::Manual, None)
    }
    fn create(
        foreground: Option<Gate>,
        capture: Option<Gate>,
        walk: Option<Gate>,
        clock_mode: Clock,
        completion: Option<Failure>,
    ) -> Self {
        let (requests, input) = mpsc::channel();
        let (output, replies) = mpsc::channel();
        let (audit, scrubbed) = mpsc::channel();
        let (report, masks) = mpsc::channel();
        let (sample, sampled) = mpsc::channel();
        let clock = Arc::new(Mutex::new(Instant::now()));
        let time = Arc::clone(&clock);
        let opened = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let opens = Arc::clone(&opened);
        let done = thread::spawn(move || {
            let now = *time.lock().unwrap();
            SOURCE.with(|source| {
                *source.borrow_mut() = Some(Setup {
                    gate: walk,
                    now,
                    masks: report,
                });
            });
            let mut foreground = foreground;
            let mut capture = capture;
            let captured_at = Arc::clone(&time);
            serve_on(
                Input {
                    receive: input,
                    current: Cursor::new(Vec::new()),
                },
                &mut Output {
                    send: output,
                    bytes: Vec::new(),
                },
                move || {
                    opens.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(Box::new(Fake {
                        clock: Arc::clone(&captured_at),
                        foreground: foreground.take(),
                        capture: capture.take(),
                        audit: audit.clone(),
                        completion,
                        sampled: sample.clone(),
                        _local: Rc::new(()),
                    }))
                },
                reader,
                move || {
                    if matches!(clock_mode, Clock::Realtime) {
                        Instant::now()
                    } else {
                        *time.lock().unwrap()
                    }
                },
                unchanged,
            )
        });
        Self {
            requests,
            replies,
            scrubbed,
            masks,
            sampled,
            clock,
            opened,
            done,
        }
    }
    fn send(&self, id: u64, body: &Request) {
        let mut bytes = Vec::new();
        write_message(&mut bytes, id, body, Kind::MAX_REQUEST).unwrap();
        self.requests.send(bytes).unwrap();
    }
    fn reply(&self) -> (u64, Reply) {
        self.replies.recv_timeout(PATIENCE).expect("worker reply")
    }
    fn acquired(&self, id: u64) {
        let (request, reply) = self.reply();
        let Reply::Acquired(actual, meta) = reply else {
            panic!("not acquired: {reply:?}")
        };
        assert_eq!(
            (request, actual, meta.capture, meta.display, meta.dpi),
            (id, key(id), CaptureId(id), RECT, 96)
        );
        assert_eq!(self.reply(), (id, Reply::Complete(key(id))));
    }
    fn scrubbed(&self) {
        loop {
            let bytes = self
                .scrubbed
                .recv_timeout(PATIENCE)
                .expect("scrub before disposal acknowledgement");
            if bytes.is_empty() {
                continue;
            }
            assert_eq!(bytes, vec![0; 48]);
            return;
        }
    }
    fn finish(self) {
        drop(self.requests);
        self.done.join().unwrap().unwrap();
    }
}

#[test]
fn drop_and_retire_are_accepted_during_foreground_or_capture_and_no_ack_precedes_scrub() {
    for phase in ["foreground", "capture"] {
        for control in ["drop", "retire"] {
            let (gate, entered, release) = gate();
            let rig = if phase == "foreground" {
                Rig::start(Some(gate), None, None)
            } else {
                Rig::start(None, Some(gate), None)
            };
            rig.send(1, &context(1));
            entered.recv_timeout(PATIENCE).unwrap();
            let request = if control == "drop" {
                Request::Drop(key(1))
            } else {
                Request::Retire(key(1).owner)
            };
            let capture = if control == "drop" {
                Some(CaptureId(1))
            } else {
                None
            };
            rig.send(2, &request);
            rig.send(3, &Request::Ping);
            assert_eq!(
                rig.reply(),
                (3, Reply::Pong),
                "controls accepted while native work is held"
            );
            assert_eq!(
                rig.replies.try_recv(),
                Err(mpsc::TryRecvError::Empty),
                "no early Ack"
            );
            release.send(()).unwrap();
            assert_eq!(
                rig.reply(),
                (1, Reply::Refused(Refusal::Stale)),
                "no late publication"
            );
            assert_eq!(rig.reply(), (2, Reply::Released(key(1).owner, capture)));
            rig.scrubbed();
            rig.send(4, &Request::Drop(key(1)));
            assert_eq!(
                rig.reply(),
                (4, Reply::Released(key(1).owner, Some(CaptureId(1))))
            );
            rig.finish();
        }
    }
}

#[test]
fn exactly_queue_release_waiters_finish_after_scrub_and_the_extra_one_is_refused() {
    let (gate, entered, release) = gate();
    let rig = Rig::start(None, Some(gate), None);
    rig.send(1, &context(1));
    entered.recv_timeout(PATIENCE).unwrap();
    for id in 2..=u64::try_from(QUEUE + 1).unwrap() {
        rig.send(id, &Request::Drop(key(1)));
    }
    let extra = u64::try_from(QUEUE + 2).unwrap();
    rig.send(extra, &Request::Drop(key(1)));
    assert_eq!(rig.reply(), (extra, Reply::Busy));
    rig.send(extra + 1, &Request::Ping);
    assert_eq!(rig.reply(), (extra + 1, Reply::Pong));
    assert_eq!(rig.replies.try_recv(), Err(mpsc::TryRecvError::Empty));
    release.send(()).unwrap();
    assert_eq!(rig.reply(), (1, Reply::Refused(Refusal::Stale)));
    for id in 2..=u64::try_from(QUEUE + 1).unwrap() {
        assert_eq!(
            rig.reply(),
            (id, Reply::Released(key(1).owner, Some(CaptureId(1))))
        );
    }
    rig.scrubbed();
    rig.finish();
}

#[test]
fn wrong_owner_and_capture_cannot_release_the_live_frame() {
    let rig = Rig::start(None, None, None);
    rig.send(1, &context(1));
    rig.acquired(1);
    let mut foreign = key(1);
    foreign.owner.gesture = 8;
    for (id, wrong) in [(2, foreign), (3, key(2))] {
        rig.send(id, &Request::Drop(wrong));
        assert_eq!(rig.reply(), (id, Reply::Refused(Refusal::Stale)));
    }
    rig.send(
        4,
        &Request::Elements {
            key: key(1),
            target: ElementTarget::Window {
                window: WindowRef(7),
                pid: 8,
            },
        },
    );
    let (id, reply) = rig.reply();
    let Reply::Elements(actual, tree) = reply else {
        panic!("not elements")
    };
    assert_eq!(
        (id, actual, tree.capture, tree.elements[0].name.as_str()),
        (4, key(1), CaptureId(1), "late_tree_content_sentinel")
    );
    rig.send(5, &Request::Drop(key(1)));
    assert_eq!(
        rig.reply(),
        (5, Reply::Released(key(1).owner, Some(CaptureId(1))))
    );
    rig.scrubbed();
    rig.finish();
}

#[test]
fn a_late_walk_is_taken_and_discarded_before_release_ack_and_the_reader_can_be_reused() {
    let (gate, entered, release) = gate();
    let rig = Rig::start(None, None, Some(gate));
    rig.send(1, &context(1));
    rig.acquired(1);
    rig.send(
        2,
        &Request::Elements {
            key: key(1),
            target: ElementTarget::Window {
                window: WindowRef(7),
                pid: 8,
            },
        },
    );
    entered.recv_timeout(PATIENCE).unwrap();
    rig.send(3, &Request::Drop(key(1)));
    rig.send(4, &Request::Ping);
    assert_eq!(rig.reply(), (4, Reply::Pong));
    assert_eq!(
        rig.replies.try_recv(),
        Err(mpsc::TryRecvError::Empty),
        "no Ack with an untaken tree"
    );
    rig.scrubbed();
    release.send(()).unwrap();
    assert_eq!(
        rig.reply(),
        (2, Reply::Refused(Refusal::Stale)),
        "the late text did not publish"
    );
    assert_eq!(
        rig.reply(),
        (3, Reply::Released(key(1).owner, Some(CaptureId(1))))
    );
    rig.send(5, &context(5));
    rig.acquired(5);
    rig.send(
        6,
        &Request::Elements {
            key: key(5),
            target: ElementTarget::Window {
                window: WindowRef(7),
                pid: 8,
            },
        },
    );
    let (id, reply) = rig.reply();
    let Reply::Elements(actual, tree) = reply else {
        panic!("not elements")
    };
    assert_eq!(
        (id, actual.capture, tree.capture),
        (6, CaptureId(5), CaptureId(5))
    );
    rig.finish();
}

#[test]
fn retirement_without_a_capture_fences_old_owners_and_a_failed_attempt_stays_spent() {
    let rig = Rig::start(None, None, None);
    rig.send(1, &Request::Retire(key(1).owner));
    assert_eq!(rig.reply(), (1, Reply::Released(key(1).owner, None)));
    rig.send(2, &context(2));
    assert_eq!(rig.reply(), (2, Reply::Refused(Refusal::Stale)));
    let newer = Owner {
        gesture: 8,
        ..key(1).owner
    };
    rig.send(3, &context_for(2, newer));
    assert_eq!(
        rig.reply(),
        (3, Reply::Refused(Refusal::Reused)),
        "the refused ticket is spent"
    );
    rig.send(4, &context_for(4, newer));
    let fresh = Key {
        owner: newer,
        capture: CaptureId(4),
    };
    let (_, reply) = rig.reply();
    assert!(matches!(reply, Reply::Acquired(actual, _) if actual == fresh));
    assert_eq!(rig.reply(), (4, Reply::Complete(fresh)));
    rig.finish();
}

#[test]
fn the_fence_orders_holds_by_epoch_before_gesture() {
    let rig = Rig::start(None, None, None);
    let fence = Owner {
        epoch: 2,
        gesture: 1,
    };
    rig.send(1, &Request::Retire(fence));
    assert_eq!(rig.reply(), (1, Reply::Released(fence, None)));
    let earlier_epoch = Owner {
        epoch: 1,
        gesture: 9,
    };
    for (id, owner) in [(2, earlier_epoch), (3, fence)] {
        rig.send(id, &context_for(id, owner));
        assert_eq!(
            rig.reply(),
            (id, Reply::Refused(Refusal::Stale)),
            "{owner:?}"
        );
    }
    let next = Key {
        owner: Owner {
            epoch: 2,
            gesture: 2,
        },
        capture: CaptureId(4),
    };
    rig.send(4, &context_for(4, next.owner));
    let (_, reply) = rig.reply();
    assert!(matches!(reply, Reply::Acquired(actual, _) if actual == next));
    assert_eq!(rig.reply(), (4, Reply::Complete(next)));
    rig.finish();
}

#[test]
fn actor_clock_expiry_scrubs_a_returning_capture_without_a_new_request() {
    let (gate, entered, release) = gate();
    let rig = Rig::start(None, Some(gate), None);
    rig.send(1, &context(1));
    entered.recv_timeout(PATIENCE).unwrap();
    *rig.clock.lock().unwrap() += Duration::from_secs(31);
    release.send(()).unwrap();
    assert_eq!(rig.reply(), (1, Reply::Refused(Refusal::Stale)));
    rig.scrubbed();
    rig.finish();
}

#[test]
fn a_failed_new_attempt_revokes_the_old_frame_and_invalid_elements_stay_spent() {
    let rig = Rig::start(None, None, None);
    rig.send(1, &context(1));
    rig.acquired(1);
    rig.send(
        2,
        &Request::Elements {
            key: key(1),
            target: ElementTarget::Window {
                window: WindowRef(0),
                pid: 8,
            },
        },
    );
    assert_eq!(rig.reply(), (2, Reply::Refused(Refusal::Invalid)));
    rig.send(
        3,
        &Request::Elements {
            key: key(1),
            target: ElementTarget::Foreground,
        },
    );
    assert_eq!(rig.reply(), (3, Reply::Refused(Refusal::Reused)));
    let mut nowhere = context(4);
    if let Request::Context { point, .. } = &mut nowhere {
        *point = PhysicalPoint([-1, 1]);
    }
    rig.send(4, &nowhere);
    assert_eq!(rig.reply(), (4, Reply::Refused(Refusal::NoWindow)));
    rig.scrubbed();
    rig.send(
        5,
        &Request::Elements {
            key: key(1),
            target: ElementTarget::Foreground,
        },
    );
    assert_eq!(rig.reply(), (5, Reply::Refused(Refusal::Stale)));
    rig.send(6, &context(4));
    assert_eq!(rig.reply(), (6, Reply::Refused(Refusal::Reused)));
    rig.finish();
}

#[test]
fn protocol_debug_redacts_the_outer_tree_and_ticket() {
    let marker = "outer_content_marker";
    let mut tree = ElementTree::unavailable(CaptureId(1), crate::screen::Unavailable::NoWindow);
    tree.elements.push(crate::screen::Element {
        id: crate::screen::ElementId {
            capture: CaptureId(1),
            index: 0,
        },
        name: marker.into(),
        control_type: 0,
        parent: None,
        name_cut: false,
        password: false,
        screen: RECT,
        in_window: RECT,
    });
    for debug in [
        format!("{:?}", Reply::Elements(key(1), tree)),
        format!("{:?}", context(1)),
    ] {
        assert!(!debug.contains(marker));
        assert!(!debug.contains(&format!("{:?}", marker.as_bytes())));
        assert!(!debug.contains(&format!("{:?}", [91_u8; 16])));
    }
}

#[test]
fn expiry_disposes_an_acquired_frame_without_any_followup_request() {
    let rig = Rig::create(None, None, None, Clock::Realtime, None);
    let mut request = context(1);
    if let Request::Context { ticket, .. } = &mut request {
        *ticket = TicketWire::new(CaptureId(1), 1, [91; 16], 2_000);
    }
    rig.send(1, &request);
    rig.acquired(1);
    rig.scrubbed();
    assert_eq!(rig.replies.try_recv(), Err(mpsc::TryRecvError::Empty));
    rig.finish();
}

#[test]
fn a_full_actor_inbox_coalesces_wakes_without_blocking_the_native_caller() {
    let (inbox, events) = mpsc::sync_channel(QUEUE);
    for _ in 0..QUEUE {
        inbox
            .try_send(Event::Wake)
            .unwrap_or_else(|_| panic!("space reserved"));
    }
    let wake = wake(inbox);
    let (returned, returning) = mpsc::channel();
    let native = thread::spawn(move || returned.send(wake()).unwrap());
    let result = returning.recv_timeout(PATIENCE);
    drop(events);
    native.join().unwrap();
    assert_eq!(result, Ok(true), "full is coalescible, not a blocking send");
}

#[test]
fn a_refused_repeat_while_capture_is_pending_prevents_that_jobs_late_publication() {
    let (gate, entered, release) = gate();
    let rig = Rig::start(None, Some(gate), None);
    rig.send(1, &context(1));
    entered.recv_timeout(PATIENCE).unwrap();
    rig.send(2, &context(1));
    assert_eq!(rig.reply(), (2, Reply::Refused(Refusal::Stale)));
    rig.send(3, &Request::Ping);
    assert_eq!(rig.reply(), (3, Reply::Pong));
    release.send(()).unwrap();
    assert_eq!(rig.reply(), (1, Reply::Refused(Refusal::Stale)));
    rig.scrubbed();
    rig.finish();
}

#[test]
fn a_wrong_capture_stamp_spends_admission_before_any_native_dispatch() {
    let rig = Rig::start(None, None, None);
    let mut wrong = context(1);
    if let Request::Context { key, .. } = &mut wrong {
        key.capture = CaptureId(2);
    }
    rig.send(1, &wrong);
    assert_eq!(rig.reply(), (1, Reply::Refused(Refusal::Stale)));
    rig.send(2, &context(1));
    assert_eq!(rig.reply(), (2, Reply::Refused(Refusal::Reused)));
    rig.finish();
}

#[test]
fn a_return_that_cannot_prove_native_cleanup_ends_the_loop_without_acknowledging_release() {
    let (gate, entered, release) = gate();
    let rig = Rig::create(
        None,
        Some(gate),
        None,
        Clock::Manual,
        Some(Failure::Retire(-7)),
    );
    rig.send(1, &context(1));
    entered.recv_timeout(PATIENCE).unwrap();
    rig.send(2, &Request::Drop(key(1)));
    rig.send(3, &Request::Ping);
    assert_eq!(rig.reply(), (3, Reply::Pong));
    release.send(()).unwrap();
    assert_eq!(
        rig.replies.recv_timeout(PATIENCE),
        Err(mpsc::RecvTimeoutError::Disconnected)
    );
    rig.scrubbed();
    rig.finish();
}

#[test]
fn both_controls_dispose_an_already_acquired_frame_and_duplicate_disposal_is_idempotent() {
    for request in [Request::Drop(key(1)), Request::Retire(key(1).owner)] {
        let rig = Rig::start(None, None, None);
        rig.send(1, &context(1));
        rig.acquired(1);
        let capture = match request {
            Request::Drop(_) => Some(CaptureId(1)),
            _ => None,
        };
        for id in [2, 3] {
            rig.send(id, &request);
            assert_eq!(rig.reply(), (id, Reply::Released(key(1).owner, capture)));
        }
        rig.scrubbed();
        rig.finish();
    }
}

#[test]
fn a_poisoned_walk_cannot_acknowledge_release_while_its_provider_still_runs() {
    let (gate, entered, release) = gate();
    let rig = Rig::start(None, None, Some(gate));
    rig.send(1, &context(1));
    rig.acquired(1);
    rig.send(
        2,
        &Request::Elements {
            key: key(1),
            target: ElementTarget::Window {
                window: WindowRef(7),
                pid: 8,
            },
        },
    );
    entered.recv_timeout(PATIENCE).unwrap();
    let masks = rig.masks.recv_timeout(PATIENCE).unwrap();
    assert!(
        thread::spawn(move || {
            let _held = masks.lock().unwrap();
            panic!("poison only the synthetic walk sink");
        })
        .join()
        .is_err()
    );
    rig.send(3, &Request::Drop(key(1)));
    let result = loop {
        let result = rig.replies.recv_timeout(PATIENCE);
        if matches!(result, Ok((2, Reply::Refused(Refusal::Stale)))) {
            continue;
        }
        break result;
    };
    let sent = release.send(());
    assert_eq!(
        result,
        Err(mpsc::RecvTimeoutError::Disconnected),
        "a retired reader has not proved its native provider stopped"
    );
    sent.expect("provider remained held until this assertion");
    rig.scrubbed();
    rig.finish();
}

#[test]
fn a_blocked_walk_does_not_queue_the_next_captures_foreground_behind_it() {
    let (gate, entered, release) = gate();
    let rig = Rig::start(None, None, Some(gate));
    rig.send(1, &context(1));
    rig.acquired(1);
    let capture_thread = rig.sampled.recv_timeout(PATIENCE).unwrap();
    rig.send(
        2,
        &Request::Elements {
            key: key(1),
            target: ElementTarget::Window {
                window: WindowRef(7),
                pid: 8,
            },
        },
    );
    entered.recv_timeout(PATIENCE).unwrap();
    rig.send(3, &context(3));
    rig.acquired(3);
    assert_eq!(
        rig.sampled.recv_timeout(PATIENCE),
        Ok(capture_thread),
        "foreground was sampled on the independent capture thread"
    );
    rig.send(4, &Request::Drop(key(3)));
    rig.send(5, &Request::Ping);
    assert_eq!(rig.reply(), (5, Reply::Pong));
    assert_eq!(
        rig.replies.try_recv(),
        Err(mpsc::TryRecvError::Empty),
        "old walk still prevents release Ack"
    );
    rig.scrubbed();
    release.send(()).unwrap();
    assert_eq!(rig.reply(), (2, Reply::Refused(Refusal::Stale)));
    assert_eq!(
        rig.reply(),
        (4, Reply::Released(key(3).owner, Some(CaptureId(3))))
    );
    rig.finish();
}

#[test]
fn a_window_fault_answers_its_hold_alone_and_the_same_capturer_serves_the_next() {
    let rig = Rig::create(None, None, None, Clock::Manual, Some(Failure::Fault(-5)));
    rig.send(1, &context(1));
    assert_eq!(rig.reply(), (1, Reply::Fault(-5)));
    rig.scrubbed();
    rig.send(2, &context(2));
    rig.acquired(2);
    assert_eq!(rig.opened.load(std::sync::atomic::Ordering::SeqCst), 1);
    rig.finish();
}

#[test]
fn a_walked_hold_seals_one_masked_jpeg_and_releases_its_frame() {
    let rig = Rig::start(None, None, None);
    rig.send(1, &context(1));
    rig.acquired(1);
    rig.send(
        2,
        &Request::Elements {
            key: key(1),
            target: ElementTarget::Foreground,
        },
    );
    let (id, reply) = rig.reply();
    assert!(
        id == 2 && matches!(reply, Reply::Elements(walked, _) if walked == key(1)),
        "{reply:?}"
    );
    rig.send(3, &Request::Seal(key(1)));
    let (id, reply) = rig.reply();
    let Reply::Sealed(sealed, context) = reply else {
        panic!("not sealed: {reply:?}")
    };
    assert_eq!((id, sealed), (3, key(1)));
    let image = image::load_from_memory_with_format(context.jpeg(), image::ImageFormat::Jpeg)
        .expect("a JPEG");
    assert_eq!((image.width(), image.height()), (4, 3));
    assert_eq!(context.transform().display, RECT);
    rig.scrubbed();
    rig.send(4, &Request::Seal(key(1)));
    assert_eq!(rig.reply(), (4, Reply::Refused(Refusal::Stale)));
    rig.finish();
}

#[test]
fn a_complete_walk_of_another_window_never_seals_the_captured_one() {
    let rig = Rig::start(None, None, None);
    rig.send(1, &context(1));
    rig.acquired(1);
    let other = ElementTarget::Window {
        window: WindowRef(9),
        pid: 8,
    };
    rig.send(
        2,
        &Request::Elements {
            key: key(1),
            target: other,
        },
    );
    let (id, reply) = rig.reply();
    assert!(
        id == 2 && matches!(reply, Reply::Elements(walked, _) if walked == key(1)),
        "{reply:?}"
    );
    rig.send(3, &Request::Seal(key(1)));
    assert_eq!(rig.reply(), (3, Reply::Refused(Refusal::Unmasked)));
    rig.scrubbed();
    rig.send(
        4,
        &Request::Elements {
            key: key(1),
            target: ElementTarget::Window {
                window: WindowRef(7),
                pid: 8,
            },
        },
    );
    assert_eq!(rig.reply(), (4, Reply::Refused(Refusal::Stale)));
    rig.send(5, &Request::Seal(key(1)));
    assert_eq!(rig.reply(), (5, Reply::Refused(Refusal::Stale)));
    rig.finish();
}

#[test]
fn sealing_before_the_walk_refuses_releases_the_frame_and_stays_refused() {
    let rig = Rig::start(None, None, None);
    rig.send(1, &context(1));
    rig.acquired(1);
    rig.send(2, &Request::Seal(key(1)));
    assert_eq!(rig.reply(), (2, Reply::Refused(Refusal::Unmasked)));
    rig.scrubbed();
    rig.send(
        3,
        &Request::Elements {
            key: key(1),
            target: ElementTarget::Foreground,
        },
    );
    assert_eq!(rig.reply(), (3, Reply::Refused(Refusal::Stale)));
    rig.send(4, &Request::Seal(key(1)));
    assert_eq!(rig.reply(), (4, Reply::Refused(Refusal::Stale)));
    rig.finish();
}
#[test]
fn a_walk_answers_only_how_complete_its_masks_are_and_the_hold_seals() {
    let rig = Rig::start(None, None, None);
    rig.send(1, &context(1));
    rig.acquired(1);
    rig.send(2, &Request::Walk { key: key(1) });
    assert_eq!(
        rig.reply(),
        (2, Reply::Walked(key(1), Completeness::Complete))
    );
    rig.send(3, &Request::Seal(key(1)));
    let (id, reply) = rig.reply();
    assert!(
        id == 3 && matches!(reply, Reply::Sealed(sealed, _) if sealed == key(1)),
        "{reply:?}"
    );
    rig.scrubbed();
    rig.finish();
}

#[test]
fn a_window_changed_since_its_capture_refuses_the_seal_and_releases_the_frame() {
    let rig = Rig::start(None, None, None);
    let Request::Context { key, ticket, .. } = context(1) else {
        unreachable!()
    };
    rig.send(
        1,
        &Request::Context {
            key,
            ticket,
            point: CHANGES,
        },
    );
    rig.acquired(1);
    rig.send(2, &Request::Walk { key });
    assert_eq!(rig.reply(), (2, Reply::Walked(key, Completeness::Complete)));
    rig.send(3, &Request::Seal(key));
    assert_eq!(rig.reply(), (3, Reply::Refused(Refusal::WindowChanged)));
    rig.scrubbed();
    rig.send(4, &Request::Seal(key));
    assert_eq!(rig.reply(), (4, Reply::Refused(Refusal::Stale)));
    rig.finish();
}

#[test]
fn a_sealed_context_never_prints_its_jpeg() {
    let marker = b"KUMOCHI-SEALED-MARKER".to_vec();
    let encoded = crate::screen::encode::Encoded {
        jpeg: zeroize::Zeroizing::new(marker.clone()),
        size: [4, 3],
        capture: CaptureId(1),
        generation: 1,
        completeness: crate::screen::mask::Completeness::Complete,
        transform: crate::screen::encode::CaptureTransform {
            display: RECT,
            dpi: 96,
            overview_size: [4, 3],
            crop: None,
        },
    };
    let context = crate::screen::context::ScreenContext::new(encoded);
    let printed = format!("{context:?}");
    let reply = format!("{:?}", Reply::Sealed(key(1), context));
    for text in [printed, reply] {
        assert!(!text.contains("KUMOCHI-SEALED-MARKER"), "{text}");
        assert!(!text.contains(&format!("{marker:?}")[1..20]), "{text}");
    }
}
