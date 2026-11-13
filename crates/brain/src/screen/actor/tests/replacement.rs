use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

const OLD: PhysicalRect = PhysicalRect {
    origin: [0, 0],
    size: [8, 8],
};
const NEW: PhysicalRect = PhysicalRect {
    origin: [0, 0],
    size: [2, 2],
};

struct Captures {
    clock: Arc<Mutex<Instant>>,
    audit: mpsc::Sender<Vec<u8>>,
    allocations: mpsc::Sender<(usize, usize)>,
    stopped: mpsc::Sender<()>,
    second: Option<Gate>,
    calls: usize,
}

impl Drop for Captures {
    fn drop(&mut self) {
        self.stopped.send(()).unwrap_or_default();
    }
}

impl Capturer for Captures {
    /// The point names the window's size, so each hold can ask for its own.
    fn pointer_window(&mut self, point: PhysicalPoint, _: u32) -> Result<Target, Refusal> {
        let [width, height] = point.0.map(|side| u32::try_from(side).unwrap());
        Ok(Target {
            top: TopWindow {
                window: WindowRef(7),
                pid: 8,
                rect: PhysicalRect {
                    origin: [0, 0],
                    size: [width, height],
                },
            },
            instance: 9,
            dpi: 96,
        })
    }
    fn capture_window(
        &mut self,
        target: &Target,
        until: Instant,
        frame: &mut RawFrame,
    ) -> Result<Acquired, Failure> {
        let display = DisplayRef(target.top.rect);
        self.capture_into(&display, Exclusion::KumochiWindows, until, frame)
    }
    fn capture_into(
        &mut self,
        display: &DisplayRef,
        _: Exclusion,
        _: Instant,
        frame: &mut RawFrame,
    ) -> Result<Acquired, Failure> {
        self.calls += 1;
        if self.calls == 2 {
            let (pointer, capacity) = frame.allocation();
            self.allocations.send((pointer.addr(), capacity)).unwrap();
            self.second.take().unwrap().wait();
        }
        frame.audit(self.audit.clone());
        frame.begin(display.0.size[0], display.0.size[1])?.fill(91);
        let (pointer, capacity) = frame.allocation();
        self.allocations.send((pointer.addr(), capacity)).unwrap();
        Ok(Acquired {
            arrived: *self.clock.lock().unwrap(),
            dpi: 96,
        })
    }
}

struct Replies {
    output: Output,
    pause: Arc<AtomicBool>,
}

impl Write for Replies {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.output.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        let reply = read_request::<Reply>(&mut self.output.bytes.as_slice(), Kind::MAX_REPLY)
            .unwrap()
            .unwrap();
        if reply == (1, Reply::Complete(key(1))) {
            self.pause.store(true, Ordering::SeqCst);
        }
        self.output.flush()
    }
}

struct Replacement {
    inbox: SyncSender<Event>,
    replies: mpsc::Receiver<(u64, Reply)>,
    scrubbed: mpsc::Receiver<Vec<u8>>,
    allocations: mpsc::Receiver<(usize, usize)>,
    stopped: mpsc::Receiver<()>,
    release_receive: mpsc::Sender<()>,
    release_capture: mpsc::Sender<()>,
    clock: Arc<Mutex<Instant>>,
    old_allocation: (usize, usize),
    handoff: (usize, usize),
    next: Key,
    done: Option<thread::JoinHandle<Result<(), frame::FrameError>>>,
}

impl Replacement {
    fn held(owner: Owner, elapsed: Duration) -> Self {
        let (inbox, events) = mpsc::sync_channel(QUEUE);
        let (send, replies) = mpsc::channel();
        let (audit, scrubbed) = mpsc::channel();
        let (allocate, allocations) = mpsc::channel();
        let (stop, stopped) = mpsc::channel();
        let (receive, receiving, release_receive) = gate();
        let (second, entering, release_capture) = gate();
        let clock = Arc::new(Mutex::new(Instant::now()));
        let time = Arc::clone(&clock);
        let wake = wake(inbox.clone());
        let done = thread::spawn(move || {
            let pause = Arc::new(AtomicBool::new(false));
            let armed = Arc::clone(&pause);
            let mut output = Replies {
                output: Output {
                    send,
                    bytes: Vec::new(),
                },
                pause,
            };
            let captured_at = Arc::clone(&time);
            let mut second = Some(second);
            let native = CaptureThread::open(
                move || {
                    Ok(Box::new(Captures {
                        clock: Arc::clone(&captured_at),
                        audit: audit.clone(),
                        allocations: allocate.clone(),
                        stopped: stop.clone(),
                        second: second.take(),
                        calls: 0,
                    }))
                },
                wake,
            )
            .unwrap();
            let receive = RefCell::new(Some(receive));
            run(&events, &mut output, native, Screen::default(), &|| {
                let now = *time.lock().unwrap();
                if armed.swap(false, Ordering::SeqCst) {
                    receive.borrow_mut().take().unwrap().wait();
                }
                now
            })
        });
        let next = Key {
            owner,
            capture: CaptureId(2),
        };
        let mut rig = Self {
            inbox,
            replies,
            scrubbed,
            allocations,
            stopped,
            release_receive,
            release_capture,
            clock,
            old_allocation: (0, 0),
            handoff: (0, 0),
            next,
            done: Some(done),
        };
        rig.context(key(1), OLD);
        rig.acquired(key(1), OLD);
        receiving.recv_timeout(PATIENCE).unwrap();
        rig.old_allocation = rig.allocations.recv_timeout(PATIENCE).unwrap();
        assert_eq!(rig.old_allocation.1, 256);
        for bytes in rig.scrubbed.try_iter() {
            assert!(bytes.is_empty());
        }
        *rig.clock.lock().unwrap() += elapsed;
        rig.context(next, NEW);
        rig.release_receive.send(()).unwrap();
        entering.recv_timeout(PATIENCE).unwrap();
        rig.handoff = rig.allocations.recv_timeout(PATIENCE).unwrap();
        rig
    }

    fn send(&self, id: u64, body: Request) {
        self.inbox
            .send(Event::Request(id, body, *self.clock.lock().unwrap()))
            .unwrap_or_else(|_| panic!("actor listens"));
    }

    fn context(&self, key: Key, display: PhysicalRect) {
        self.send(
            key.capture.0,
            Request::Context {
                key,
                ticket: TicketWire::new(RunId(3), key.capture, key.capture.0, [91; 16], 30_000),
                point: PhysicalPoint(display.size.map(|side| i32::try_from(side).unwrap())),
            },
        );
    }

    fn acquired(&self, key: Key, display: PhysicalRect) {
        assert_eq!(
            self.replies.recv_timeout(PATIENCE).unwrap(),
            (
                key.capture.0,
                Reply::Acquired(
                    key,
                    FrameMeta {
                        capture: key.capture,
                        display,
                        dpi: 96,
                        acquired_after_ms: 0,
                    }
                )
            )
        );
        assert_eq!(
            self.replies.recv_timeout(PATIENCE).unwrap(),
            (key.capture.0, Reply::Complete(key))
        );
    }

    fn disposed_before_handoff(&self) {
        assert_eq!(
            self.handoff.1, 0,
            "outgoing allocation reached the second native invocation"
        );
        let bytes = self
            .scrubbed
            .try_recv()
            .expect("scrub preceded storage handoff");
        assert_eq!(bytes.len(), 256);
        assert!(
            bytes.iter().all(|byte| *byte == 0),
            "whole old storage scrubbed"
        );
    }

    fn retire_old_and_complete(&self) {
        self.send(3, Request::Retire(key(1).owner));
        self.send(4, Request::Ping);
        assert_eq!(
            self.replies.recv_timeout(PATIENCE).unwrap(),
            (4, Reply::Pong)
        );
        assert_eq!(self.replies.try_recv(), Err(mpsc::TryRecvError::Empty));
        self.release_capture.send(()).unwrap();
        self.acquired(self.next, NEW);
        assert_eq!(
            self.replies.recv_timeout(PATIENCE).unwrap(),
            (3, Reply::Released(key(1).owner, None))
        );
    }
}

impl Drop for Replacement {
    fn drop(&mut self) {
        self.release_receive.send(()).unwrap_or_default();
        self.release_capture.send(()).unwrap_or_default();
        drop(self.inbox.send(Event::End(Ok(()))));
        if let Some(done) = self.done.take() {
            let result = done.join();
            if !thread::panicking() {
                result.unwrap().unwrap();
            }
        }
        let stopped = self.stopped.recv_timeout(PATIENCE);
        if !thread::panicking() {
            assert_eq!(stopped, Ok(()), "synthetic capture producer stopped");
        }
    }
}

#[test]
fn expired_same_owner_replacement_disposes_before_second_capture() {
    let rig = Replacement::held(key(2).owner, Duration::from_secs(31));
    rig.disposed_before_handoff();
    rig.release_capture.send(()).unwrap();
    rig.acquired(rig.next, NEW);
}

#[test]
fn replacement_by_a_later_gesture_disposes_before_handoff_and_old_release() {
    let owner = Owner {
        gesture: 8,
        ..key(2).owner
    };
    let rig = Replacement::held(owner, Duration::ZERO);
    rig.disposed_before_handoff();
    rig.retire_old_and_complete();
}

#[test]
fn replacement_by_a_later_epoch_disposes_before_handoff_and_old_release() {
    let owner = Owner {
        epoch: 2,
        gesture: 1,
    };
    let rig = Replacement::held(owner, Duration::ZERO);
    rig.disposed_before_handoff();
    rig.retire_old_and_complete();
}

#[test]
fn unexpired_same_owner_replacement_reuses_the_allocation() {
    let rig = Replacement::held(key(2).owner, Duration::ZERO);
    assert_eq!(rig.handoff, rig.old_allocation);
    assert_eq!(rig.scrubbed.try_recv(), Err(mpsc::TryRecvError::Empty));
    rig.release_capture.send(()).unwrap();
    rig.acquired(rig.next, NEW);
    assert_eq!(
        rig.allocations.recv_timeout(PATIENCE).unwrap(),
        rig.old_allocation
    );
}
