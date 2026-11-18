//! The output owner against the real worker, engine and serve loop on a scripted device.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use kumochi_protocol::message::{
    ControlToBrain, Cursor, OutputGeneration, RunId, SegmentId, SpeechPcm,
};

use super::{Abort, OutputKey, OutputOwner, State, Why};
use crate::speaker::fake::{self, Script};
use crate::speaker::{Command, Data, Input, Outlet, Packet, Snapshot, Speaker};

const PATIENCE: Duration = Duration::from_secs(5);

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !done() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn state(owner: &OutputOwner) -> MutexGuard<'_, State> {
    owner.state.lock().expect("unpoisoned")
}

/// The ledger's last snapshot of `generation`.
fn snapshot(owner: &OutputOwner, generation: OutputGeneration) -> Option<Snapshot> {
    state(owner)
        .ledger
        .iter()
        .flatten()
        .find(|entry| entry.generation == generation)
        .and_then(|entry| entry.tail.read())
}

fn abort(aborted: &Arc<AtomicBool>) -> Abort {
    let aborted = Arc::clone(aborted);
    Arc::new(move || aborted.store(true, Ordering::SeqCst))
}

/// One session whose speaker plays on a scripted device; dropping it ends the session.
struct Rig<'a> {
    owner: &'a OutputOwner,
    session: u64,
    generation: OutputGeneration,
    speaker: Option<Speaker>,
    input: Input,
    script: fake::Shared,
    brain: async_channel::Receiver<ControlToBrain>,
    granted: Arc<AtomicU32>,
    ended: Arc<AtomicBool>,
    aborted: Arc<AtomicBool>,
}

fn rig(owner: &OutputOwner, session: u64) -> Rig<'_> {
    let aborted = Arc::new(AtomicBool::new(false));
    let (generation, tail) = owner
        .begin(session, Some(abort(&aborted)))
        .expect("a slot and a generation");
    let (control, brain) = async_channel::bounded(64);
    let granted = Arc::new(AtomicU32::new(0));
    let script = fake::Shared::default();
    let outlet = Outlet {
        generation,
        control,
        grant: Box::new({
            let granted = Arc::clone(&granted);
            move |bytes| {
                granted.fetch_add(bytes, Ordering::SeqCst);
            }
        }),
        tail,
        wake: owner.waker(),
    };
    let mut speaker = owner
        .opened(generation, fake::open(outlet, Arc::clone(&script)))
        .expect("fenced")
        .expect("opened");
    let ended = Arc::new(AtomicBool::new(false));
    speaker.arm(Box::new({
        let ended = Arc::clone(&ended);
        move || ended.store(true, Ordering::SeqCst)
    }));
    let input = speaker.input().expect("the reader's side");
    Rig {
        owner,
        session,
        generation,
        speaker: Some(speaker),
        input,
        script,
        brain,
        granted,
        ended,
        aborted,
    }
}

impl Rig<'_> {
    /// Names `run` and queues one segment of `samples`.
    fn speak(&self, run: RunId, samples: Vec<i16>) {
        let speaker = self.speaker.as_ref().expect("held");
        assert!(speaker.command(Command::Name(run)), "named");
        let last = Some(u32::try_from(samples.len()).expect("a chunk"));
        let queued = Data::Queued {
            run,
            segment: SegmentId(0),
        };
        let pcm = Data::Pcm(SpeechPcm {
            run,
            segment: SegmentId(0),
            output_generation: self.generation,
            seq: 0,
            samples,
            last,
        });
        for (bytes, data) in [(1, queued), (2, pcm)] {
            self.input.send(Packet { bytes, data }).expect("taken");
        }
    }

    fn script(&self) -> MutexGuard<'_, Script> {
        fake::lock(&self.script)
    }

    fn snapshot(&self) -> Option<Snapshot> {
        snapshot(self.owner, self.generation)
    }

    /// The generation's snapshot, read again past any write it overlapped.
    fn coherent(&self) -> Snapshot {
        let mut read = None;
        until("a coherent read", || {
            read = self.snapshot();
            read.is_some()
        });
        read.expect("read")
    }

    /// Writes `samples` of content and plays all but its last frame; returns its end.
    fn short(&self, samples: usize) -> u64 {
        self.speak(RunId(1), vec![7; samples]);
        until("the speech was written and committed", || {
            self.script().content.len() == samples
                && self.snapshot().is_some_and(|now| now.flight == 0)
        });
        let exposed = self.coherent().exposed;
        self.script().play_to = exposed - 1;
        until("played to one frame short", || {
            self.snapshot().is_some_and(|now| now.played == exposed - 1)
        });
        exposed
    }

    fn heard(&self, report: &ControlToBrain) -> bool {
        std::iter::from_fn(|| self.brain.try_recv().ok()).any(|sent| sent == *report)
    }
}

impl Drop for Rig<'_> {
    fn drop(&mut self) {
        self.owner.end(self.session);
        drop(self.speaker.take());
    }
}

fn retired(owner: &OutputOwner) -> bool {
    owner.view().why == Some(Why::Retired)
}

#[test]
fn the_responder_answers_a_revoke_from_before_quiet_and_one_after_retirement() {
    let owner = OutputOwner::default();
    let rig = rig(&owner, 1);
    rig.short(480);
    assert!(owner.stop(1, RunId(1)), "queued");
    std::thread::sleep(Duration::from_millis(50));
    let answered = ControlToBrain::SpeechStopped {
        run: RunId(1),
        cursor: Some(Cursor {
            segment: SegmentId(0),
            sample: 480,
        }),
    };
    assert!(!rig.heard(&answered), "not before its last frame played");
    rig.script().frozen = true;
    until("parked", || rig.script().parked);
    owner.withdraw();
    {
        let mut script = rig.script();
        script.play_to = u64::MAX;
        script.frozen = false;
    }
    until("answered once played", || rig.heard(&answered));
    until("retired", || retired(&owner));
    assert!(owner.stop(1, RunId(2)), "queued on the responder");
    let later = ControlToBrain::SpeechStopped {
        run: RunId(2),
        cursor: None,
    };
    until("the responder answers", || rig.heard(&later));
    assert!(
        !rig.ended.load(Ordering::SeqCst) && !rig.aborted.load(Ordering::SeqCst),
        "the worker and the session go on"
    );
}

#[test]
fn the_stop_result_never_settles_a_tail_on_its_own() {
    for (stop_fails, short, terminal) in [
        (true, false, false),
        (true, true, true),
        (false, true, true),
    ] {
        let owner = OutputOwner::default();
        let rig = rig(&owner, 1);
        rig.script().stop_fails = stop_fails;
        rig.short(480);
        if !short {
            rig.script().play_to = u64::MAX;
            until("played out", || {
                rig.snapshot().is_some_and(|now| !now.owed())
            });
        }
        owner.withdraw();
        until("settled", || {
            let view = owner.view();
            view.terminal || view.why == Some(Why::Retired)
        });
        assert_eq!(
            owner.view().terminal,
            terminal,
            "stop fails: {stop_fails}, one frame short: {short}"
        );
        assert!(rig.script().stopped, "stopped once");
    }
}

#[test]
fn a_worker_that_panics_mid_release_leaves_terminal_debt() {
    let owner = OutputOwner::default();
    let rig = rig(&owner, 1);
    rig.script().panic = true;
    rig.speak(RunId(1), vec![7; 240]);
    until("terminal", || owner.view().terminal);
}

#[test]
fn a_withdrawal_plays_out_what_was_written_before_it_stops_the_stream() {
    let owner = OutputOwner::default();
    let rig = rig(&owner, 1);
    rig.short(480);
    rig.script().frozen = true;
    until("parked", || rig.script().parked);
    owner.withdraw();
    rig.script().frozen = false;
    std::thread::sleep(Duration::from_millis(50));
    assert!(!rig.script().stopped, "still playing out");
    rig.script().play_to = u64::MAX;
    until("retired", || retired(&owner));
    assert!(!owner.view().terminal, "every frame written played");
    assert!(rig.script().stopped, "stopped once played");
}

#[test]
fn a_refused_open_whose_thread_outlives_its_caller_settles_on_its_own_last_wake() {
    let owner = OutputOwner::default();
    let (generation, tail) = owner.begin(1, Some(Arc::new(|| {}))).expect("reserved");
    let (release, held) = mpsc::channel::<()>();
    let outlet = Outlet {
        generation,
        control: async_channel::bounded(1).0,
        grant: Box::new(|_| {}),
        tail,
        wake: owner.waker(),
    };
    let refused = crate::speaker::launch(outlet, Duration::from_millis(50), move |worker, _| {
        let _released = held.recv();
        drop(worker);
    });
    assert!(refused.is_none(), "not started in time");
    assert!(
        matches!(owner.opened(generation, refused), Ok(None)),
        "refused"
    );
    assert!(owner.view().retiring, "its thread still holds the tail");
    let changes = owner.changes();
    while changes.try_recv().is_ok() {}
    release.send(()).expect("still held");
    let (woke, woken) = mpsc::channel();
    std::thread::spawn(move || woke.send(changes.recv_blocking().is_ok()));
    assert_eq!(woken.recv_timeout(PATIENCE), Ok(true), "its last wake");
    let view = owner.view();
    assert!(
        !view.retiring && !view.terminal,
        "settled clean: nothing was exposed"
    );
}

#[test]
fn an_old_generation_settling_late_never_clears_a_newer_ones_output() {
    let owner = OutputOwner::default();
    let old = rig(&owner, 1);
    old.script().frozen = true;
    until("parked", || old.script().parked);
    let old_script = Arc::clone(&old.script);
    drop(old);
    let new = rig(&owner, 2);
    new.short(480);
    assert!(
        owner.view().retiring,
        "the old generation's worker is stuck"
    );
    fake::lock(&old_script).frozen = false;
    until("the old generation settled", || !owner.view().retiring);
    assert_eq!(
        owner.view().key,
        Some(OutputKey::Generation(new.generation)),
        "the newer generation still owes"
    );
    assert!(new.coherent().owed(), "owes");
}

#[test]
fn the_fifth_unresolved_generation_is_refused_before_it_opens() {
    let owner = OutputOwner::default();
    let held: Vec<_> = (1..=4)
        .map(|session| {
            let begun = owner.begin(session, Some(Arc::new(|| {}))).expect("a slot");
            owner.end(session);
            begun
        })
        .collect();
    assert!(
        owner.begin(5, Some(Arc::new(|| {}))).is_none(),
        "no slot left"
    );
    assert_eq!(owner.view().why, Some(Why::Refused), "refused");
    let ledger: Vec<_> = state(&owner)
        .ledger
        .iter()
        .flatten()
        .map(|entry| (entry.generation, Arc::clone(&entry.tail)))
        .collect();
    assert_eq!(ledger.len(), 4, "every entry kept");
    for ((generation, tail), (kept, kept_tail)) in held.iter().zip(&ledger) {
        assert!(
            generation == kept && Arc::ptr_eq(tail, kept_tail),
            "unchanged evidence"
        );
    }
    drop((held, ledger));
    owner.end(5);
    assert!(
        owner.begin(6, Some(Arc::new(|| {}))).is_some(),
        "settled slots are free again"
    );
}
