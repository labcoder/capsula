//! Bounded test recording on the existing dropper, without changing its transport.

use std::{
    cell::RefCell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender, TrySendError},
    },
    thread::{self, ThreadId},
    time::{Duration, Instant},
};

use kumochi_scene::deferred::{self, Deferred};

use crate::allocation::{
    self,
    scopes::{FinishedRequests, LocalRequests, PreparedRequests},
};

const WAIT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    NotRunning,
    WrongThread,
    Ownership,
    Order,
    Scope,
    Timeout,
    Acknowledgement,
    ReplyLost,
}

pub(crate) enum Ack {
    Started,
    GateEntered,
    Barrier,
    Finished(FinishedRequests),
    Refused(Failure),
}

struct Reply {
    nonce: u64,
    thread: ThreadId,
    ack: Ack,
}

struct Control {
    nonce: u64,
    origin: ThreadId,
    reply: SyncSender<Reply>,
    sealed: AtomicBool,
    failed: AtomicBool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Started,
    Gate,
    Barrier,
}

struct Active {
    control: Arc<Control>,
    thread: ThreadId,
    stage: Stage,
    local: LocalRequests,
}

thread_local! {
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
}

enum Command {
    Start(PreparedRequests),
    Gate(Receiver<()>),
    Barrier,
    Finish,
}

struct Sentinel {
    nonce: u64,
    control: Arc<Control>,
    command: Option<Command>,
}

impl Sentinel {
    fn carrier(&self) {
        allocation::handoff(
            std::ptr::from_ref(self).addr(),
            size_of::<Self>(),
            align_of::<Self>(),
        );
    }

    fn apply(&self, command: Command) -> Result<Ack, Failure> {
        let current = thread::current().id();
        if current == self.control.origin {
            return Err(Failure::WrongThread);
        }
        if self.nonce == 0 || self.nonce != self.control.nonce {
            return Err(Failure::Ownership);
        }
        let result = ACTIVE.try_with(|active| {
            let Ok(mut slot) = active.try_borrow_mut() else {
                return Err(Failure::Scope);
            };
            if let Command::Start(prepared) = command {
                if slot.is_some() || self.control.sealed.load(Ordering::SeqCst) {
                    return Err(Failure::Order);
                }
                let local = prepared.install().map_err(|_| Failure::Scope)?;
                *slot = Some(Active {
                    control: Arc::clone(&self.control),
                    thread: current,
                    stage: Stage::Started,
                    local,
                });
                allocation::checkpoint(6, 240);
                self.carrier();
                return Ok(Ack::Started);
            }
            let Some(owned) = slot.as_mut() else {
                return Err(Failure::Ownership);
            };
            if !Arc::ptr_eq(&owned.control, &self.control)
                || owned.control.nonce != self.nonce
                || owned.thread != current
            {
                return Err(Failure::Ownership);
            }
            match command {
                Command::Gate(gate) => {
                    if owned.stage != Stage::Started || self.control.sealed.load(Ordering::SeqCst) {
                        return Err(Failure::Order);
                    }
                    owned.stage = Stage::Gate;
                    drop(slot);
                    allocation::checkpoint(6, 241);
                    self.carrier();
                    self.reply(Ack::GateEntered);
                    match gate.recv_timeout(WAIT) {
                        Err(mpsc::RecvTimeoutError::Disconnected) => Ok(Ack::GateEntered),
                        Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {
                            self.control.failed.store(true, Ordering::SeqCst);
                            Err(Failure::Timeout)
                        }
                    }
                }
                Command::Barrier => {
                    if !self.control.sealed.load(Ordering::SeqCst)
                        || !matches!(owned.stage, Stage::Started | Stage::Gate)
                    {
                        return Err(Failure::Order);
                    }
                    owned.stage = Stage::Barrier;
                    allocation::checkpoint(6, 242);
                    self.carrier();
                    Ok(Ack::Barrier)
                }
                Command::Finish => {
                    if owned.stage != Stage::Barrier || !self.control.sealed.load(Ordering::SeqCst)
                    {
                        return Err(Failure::Order);
                    }
                    allocation::checkpoint(6, 243);
                    self.carrier();
                    let finished = owned.local.finish().map_err(|_| Failure::Scope)?;
                    let previous = slot.take();
                    drop(slot);
                    drop(previous);
                    Ok(Ack::Finished(finished))
                }
                Command::Start(_) => Err(Failure::Order),
            }
        });
        result.unwrap_or(Err(Failure::Scope))
    }

    fn reply(&self, ack: Ack) {
        let reply = Reply {
            nonce: self.nonce,
            thread: thread::current().id(),
            ack,
        };
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) =
            self.control.reply.try_send(reply)
        {
            self.control.failed.store(true, Ordering::SeqCst);
        }
    }
}

impl Drop for Sentinel {
    fn drop(&mut self) {
        let Some(command) = self.command.take() else {
            return;
        };
        let gated = matches!(command, Command::Gate(_));
        match self.apply(command) {
            Ok(Ack::GateEntered) if gated => {}
            Ok(ack) => self.reply(ack),
            Err(error) => self.reply(Ack::Refused(error)),
        }
    }
}

pub(crate) struct GateRelease {
    _sender: Sender<()>,
}

pub(crate) struct Recorder {
    control: Arc<Control>,
    replies: Receiver<Reply>,
    prepared: Option<PreparedRequests>,
    release: Option<Sender<()>>,
    gate: Option<Receiver<()>>,
    start_enqueued: bool,
    dropper: Option<ThreadId>,
    closed: bool,
}

impl Recorder {
    pub(crate) fn prepare(nonce: u64) -> (Self, PreparedRequests, Vec<allocation::Request>) {
        let ((recorder, ui), inventory) = allocation::requests(64, || {
            let (reply, replies) = mpsc::sync_channel(8);
            let (release, gate) = mpsc::channel();
            let control = Arc::new(Control {
                nonce,
                origin: thread::current().id(),
                reply,
                sealed: AtomicBool::new(false),
                failed: AtomicBool::new(false),
            });
            (
                Self {
                    control,
                    replies,
                    prepared: Some(PreparedRequests::new()),
                    release: Some(release),
                    gate: Some(gate),
                    start_enqueued: false,
                    dropper: None,
                    closed: false,
                },
                PreparedRequests::new(),
            )
        });
        (recorder, ui, inventory)
    }

    fn enqueue(&self, nonce: u64, command: Command) -> Result<(), Failure> {
        if thread::current().id() != self.control.origin {
            return Err(Failure::WrongThread);
        }
        if !deferred::running() {
            return Err(Failure::NotRunning);
        }
        drop(Deferred::new(
            Sentinel {
                nonce,
                control: Arc::clone(&self.control),
                command: Some(command),
            },
            0,
        ));
        Ok(())
    }

    fn receive(&mut self) -> Result<Ack, Failure> {
        self.receive_until(Instant::now() + WAIT)
    }

    fn receive_until(&mut self, deadline: Instant) -> Result<Ack, Failure> {
        let reply = self
            .replies
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| Failure::Acknowledgement)?;
        if reply.nonce != self.control.nonce || reply.thread == self.control.origin {
            return Err(Failure::Ownership);
        }
        if let Some(dropper) = self.dropper
            && dropper != reply.thread
        {
            return Err(Failure::WrongThread);
        }
        if let Ack::Refused(error) = reply.ack {
            return Err(error);
        }
        Ok(reply.ack)
    }

    pub(crate) fn start(&mut self) -> Result<(), Failure> {
        let prepared = self.prepared.take().ok_or(Failure::Order)?;
        self.enqueue(self.control.nonce, Command::Start(prepared))?;
        self.start_enqueued = true;
        let reply = self
            .replies
            .recv_timeout(WAIT)
            .map_err(|_| Failure::Acknowledgement)?;
        if reply.nonce != self.control.nonce || reply.thread == self.control.origin {
            return Err(Failure::Ownership);
        }
        match reply.ack {
            Ack::Started => {
                self.dropper = Some(reply.thread);
                Ok(())
            }
            Ack::Refused(error) => Err(error),
            _ => Err(Failure::Order),
        }
    }

    pub(crate) fn hold(&mut self) -> Result<GateRelease, Failure> {
        let gate = self.gate.take().ok_or(Failure::Order)?;
        self.enqueue(self.control.nonce, Command::Gate(gate))?;
        if !matches!(self.receive()?, Ack::GateEntered) {
            return Err(Failure::Order);
        }
        let sender = self.release.take().ok_or(Failure::Order)?;
        Ok(GateRelease { _sender: sender })
    }

    fn finish_inner(&mut self) -> Result<FinishedRequests, Failure> {
        let deadline = Instant::now() + WAIT;
        drop(self.release.take());
        self.control.sealed.store(true, Ordering::SeqCst);
        let mut failure = self.enqueue(self.control.nonce, Command::Barrier).err();
        if failure.is_none() {
            match self.receive_until(deadline) {
                Ok(Ack::Barrier) => {}
                Ok(_) => failure = Some(Failure::Order),
                Err(error) => failure = Some(error),
            }
        }
        let sent = self.enqueue(self.control.nonce, Command::Finish);
        if let Some(error) = failure {
            if sent.is_ok() {
                let _cleanup = self.receive_until(deadline);
            }
            return Err(error);
        }
        sent?;
        let Ack::Finished(finished) = self.receive_until(deadline)? else {
            return Err(Failure::Order);
        };
        if self.control.failed.load(Ordering::SeqCst) {
            return Err(Failure::ReplyLost);
        }
        Ok(finished)
    }

    pub(crate) fn finish(&mut self) -> Result<FinishedRequests, Failure> {
        let result = self.finish_inner();
        self.closed = true;
        result
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        drop(self.release.take());
        if self.start_enqueued && !self.closed {
            let result = self.finish_inner();
            if result.is_err() {
                self.control.failed.store(true, Ordering::SeqCst);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(ui: PreparedRequests) -> LocalRequests {
        match ui.install() {
            Ok(local) => local,
            Err(_) => panic!("a vacant UI recorder accepts prepared storage"),
        }
    }

    fn free_after_gate(record: &FinishedRequests, pointer: usize) -> allocation::Request {
        let start = record
            .requests
            .iter()
            .position(|r| (r.action, r.tag) == (6, 241))
            .expect("the actual dropper entered Gate");
        let end = record
            .requests
            .iter()
            .position(|r| (r.action, r.tag) == (6, 242))
            .expect("the final barrier follows all permitted sends");
        assert!(start < end);
        for event in &record.requests[start..end] {
            if event.pointer == pointer {
                assert_ne!(event.action, 0, "a live transferred owner cannot be reused");
                if event.action == 1 {
                    return *event;
                }
            }
        }
        panic!("the transferred lifetime must end before the final barrier");
    }

    #[test]
    #[ignore = "requires one exact --ignored selector in a fresh isolated process"]
    fn the_actual_dropper_records_the_free_of_a_ui_created_owner() {
        deferred::start().expect("the actual dropper runs before recording");
        let (mut recorder, ui, inventory) = Recorder::prepare(1);
        let storage = ui.storage().expect("prepared UI storage");
        let dropper_storage = recorder
            .prepared
            .as_ref()
            .and_then(PreparedRequests::storage)
            .expect("prepared dropper storage");
        assert_ne!(storage.0, dropper_storage.0);
        assert_eq!(storage.1 / size_of::<allocation::Request>(), 16_384);
        assert_eq!(dropper_storage.1 / size_of::<allocation::Request>(), 16_384);
        assert!(inventory.iter().any(|event| {
            (event.action, event.pointer, event.size, event.align)
                == (0, storage.0, storage.1, storage.2)
        }));
        assert!(inventory.iter().any(|event| {
            (event.action, event.pointer, event.size, event.align)
                == (0, dropper_storage.0, dropper_storage.1, dropper_storage.2)
        }));
        let mut ui = install(ui);
        recorder.start().expect("matching actual-dropper Started");
        let gate = recorder.hold().expect("actual-dropper GateEntered");
        let value = std::hint::black_box(vec![0_u8; 137].into_boxed_slice());
        let pointer = value.as_ptr().addr();
        allocation::handoff(pointer, 137, 1);
        drop(Deferred::new(value, 137));
        assert_eq!(deferred::pending().values, 2);
        assert_eq!(deferred::pending().bytes, 137);
        drop(gate);
        let dropper = recorder.finish().expect("sealed final barrier then Finish");
        let ui = ui.finish().expect("normal UI recording return");
        assert!(!ui.overflow && !ui.lost);
        assert!(!dropper.overflow && !dropper.lost);
        let handoff = ui
            .requests
            .iter()
            .position(|r| r.action == 4 && r.pointer == pointer)
            .expect("the explicit ownership transfer");
        let created = ui.requests[..handoff]
            .iter()
            .rfind(|r| r.action == 0 && r.pointer == pointer)
            .expect("covered UI creation before transfer");
        assert_eq!((created.size, created.align, created.tag), (137, 1, 1));
        let freed = free_after_gate(&dropper, pointer);
        assert_eq!((freed.action, freed.size, freed.tag), (1, 137, 1));
        assert_ne!(recorder.dropper, Some(thread::current().id()));
    }

    #[test]
    #[ignore = "requires one exact --ignored selector in a fresh isolated process"]
    fn duplicate_start_and_early_or_foreign_finish_leave_the_installed_scope_owned() {
        deferred::start().expect("the dropper already runs");
        let duplicate = PreparedRequests::new();
        let (mut recorder, ui, _inventory) = Recorder::prepare(2);
        let mut ui = install(ui);
        recorder.start().expect("Started");
        recorder
            .enqueue(2, Command::Start(duplicate))
            .expect("duplicate delivery");
        assert!(matches!(recorder.receive(), Err(Failure::Order)));
        recorder
            .enqueue(2, Command::Finish)
            .expect("early Finish delivery");
        assert!(matches!(recorder.receive(), Err(Failure::Order)));
        recorder
            .enqueue(3, Command::Finish)
            .expect("foreign Finish delivery");
        let reply = recorder
            .replies
            .recv_timeout(WAIT)
            .expect("foreign refusal");
        assert_eq!(reply.nonce, 3);
        assert!(matches!(reply.ack, Ack::Refused(Failure::Ownership)));
        let gate = recorder
            .hold()
            .expect("the original scope remains installed");
        let value = vec![0_u8; 29].into_boxed_slice();
        let pointer = value.as_ptr().addr();
        drop(Deferred::new(value, 29));
        drop(gate);
        let finished = recorder.finish().expect("the matching Finish still works");
        assert!(!finished.lost && !finished.overflow);
        assert_eq!(free_after_gate(&finished, pointer).size, 29);
        assert!(!ui.finish().expect("UI finish").lost);
    }

    #[test]
    #[ignore = "requires one exact --ignored selector in a fresh isolated process"]
    fn unwind_releases_the_gate_and_cleanup_preserves_the_original_failure() {
        deferred::start().expect("the dropper already runs");
        let (mut recorder, ui, _inventory) = Recorder::prepare(4);
        let mut ui = install(ui);
        recorder.start().expect("Started before affected owners");
        let pointer = std::cell::Cell::new(0);
        let original = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _gate = recorder.hold().expect("GateEntered");
            let value = vec![0_u8; 41].into_boxed_slice();
            pointer.set(value.as_ptr().addr());
            drop(Deferred::new(value, 41));
            panic!("owned body failure");
        }));
        let cleaned = recorder.finish();
        let original = original.expect_err("the original failure remains failed");
        assert_eq!(
            original.downcast_ref::<&str>().copied(),
            Some("owned body failure")
        );
        let finished = cleaned.expect("bounded cleanup can restore after body failure");
        assert!(!finished.lost && !finished.overflow);
        assert_eq!(free_after_gate(&finished, pointer.get()).size, 41);
        assert!(!ui.finish().expect("UI finish").lost);
    }

    #[test]
    #[ignore = "requires one exact --ignored selector in a fresh isolated process"]
    #[should_panic(expected = "lost acknowledgement: retire this isolated image")]
    fn a_failed_reply_send_remains_sticky_after_matching_finish() {
        deferred::start().expect("the dropper already runs");
        let (mut recorder, ui, _inventory) = Recorder::prepare(5);
        let _ui = install(ui);
        recorder.start().expect("an actual installed Start");
        for _ in 0..9 {
            recorder
                .enqueue(5, Command::Finish)
                .expect("early Finish delivery");
        }
        let marker = Arc::new(());
        drop(Deferred::new(Arc::clone(&marker), 0));
        assert!(crate::presenter::tests::drained(&marker, 1));
        assert!(recorder.control.failed.load(Ordering::SeqCst));
        for _ in 0..8 {
            assert!(matches!(recorder.receive(), Err(Failure::Order)));
        }
        let original = match recorder.finish() {
            Err(error) => error,
            Ok(_) => panic!("lost reply must invalidate matching Finish"),
        };
        assert_eq!(original, Failure::ReplyLost);
        assert!(recorder.closed);
        assert!(recorder.control.failed.load(Ordering::SeqCst));
        panic!("lost acknowledgement: retire this isolated image");
    }

    #[test]
    #[ignore = "requires one exact --ignored selector in a fresh isolated process"]
    #[should_panic(expected = "missing matching Finish: retire this isolated image")]
    fn a_lost_finish_acknowledgement_requires_failed_image_retirement() {
        deferred::start().expect("the dropper already runs");
        let (mut recorder, ui, _inventory) = Recorder::prepare(6);
        let _ui = install(ui);
        recorder.start().expect("an actual installed Start");
        let (closed, receiver) = mpsc::sync_channel(1);
        drop(closed);
        let previous = std::mem::replace(&mut recorder.replies, receiver);
        drop(previous);
        let original = match recorder.finish() {
            Err(error) => error,
            Ok(_) => panic!("no matching Finish can cross the closed channel"),
        };
        let marker = Arc::new(());
        drop(Deferred::new(Arc::clone(&marker), 0));
        assert!(crate::presenter::tests::drained(&marker, 1));
        assert_eq!(original, Failure::Acknowledgement);
        assert!(recorder.control.failed.load(Ordering::SeqCst));
        assert!(recorder.closed);
        panic!("missing matching Finish: retire this isolated image");
    }

    #[test]
    #[ignore = "requires one exact --ignored selector in a fresh isolated process"]
    #[should_panic(expected = "unacknowledged Start: retire this isolated image")]
    fn an_installed_unacknowledged_start_attempts_abandonment_cleanup() {
        struct Installed {
            control: Arc<Control>,
            observed: Arc<AtomicBool>,
        }
        impl Drop for Installed {
            fn drop(&mut self) {
                let installed = ACTIVE
                    .try_with(|slot| {
                        slot.try_borrow().is_ok_and(|active| {
                            active.as_ref().is_some_and(|owned| {
                                Arc::ptr_eq(&owned.control, &self.control)
                                    && owned.stage == Stage::Started
                            })
                        })
                    })
                    .unwrap_or(false);
                self.observed.store(installed, Ordering::SeqCst);
            }
        }

        deferred::start().expect("the actual dropper already runs");
        let (mut recorder, ui, _inventory) = Recorder::prepare(7);
        let _ui = install(ui);
        let control = Arc::clone(&recorder.control);
        let observed = Arc::new(AtomicBool::new(false));
        let (closed, receiver) = mpsc::sync_channel(1);
        drop(closed);
        drop(std::mem::replace(&mut recorder.replies, receiver));
        let original = recorder
            .start()
            .expect_err("Started cannot cross the closed channel");
        drop(Deferred::new(
            Installed {
                control: Arc::clone(&control),
                observed: Arc::clone(&observed),
            },
            0,
        ));
        let marker = Arc::new(());
        drop(Deferred::new(Arc::clone(&marker), 0));
        assert!(crate::presenter::tests::drained(&marker, 1));
        assert!(observed.load(Ordering::SeqCst));
        assert!(recorder.start_enqueued);
        assert_eq!(recorder.dropper, None);
        assert!(control.failed.load(Ordering::SeqCst));
        assert!(!control.sealed.load(Ordering::SeqCst));
        drop(recorder);
        let marker = Arc::new(());
        drop(Deferred::new(Arc::clone(&marker), 0));
        assert!(crate::presenter::tests::drained(&marker, 1));
        assert!(control.sealed.load(Ordering::SeqCst));
        assert!(control.failed.load(Ordering::SeqCst));
        assert_eq!(original, Failure::Acknowledgement);
        panic!("unacknowledged Start: retire this isolated image");
    }
}
