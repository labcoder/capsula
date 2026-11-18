//! The output owner against the real worker, engine and serve loop on a scripted device.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use kumochi_protocol::message::{
    ControlToBrain, Cursor, GestureId, MicPcm, OutputGeneration, RunId, SegmentId, SpeechPcm,
};

use super::{Abort, Claim, Denied, InhibitToken, OutputKey, OutputOwner, Retirement, State, Why};
use crate::platform::microphone::{
    CollectionGate, Finalization, Generation, MicError, MicOwner, Mode, Running,
};
use crate::speaker::fake::{self, Script};
use crate::speaker::{
    Command, Data, FAULT, Input, Outlet, Packet, RELEASED, Snapshot, Speaker, UNKNOWN,
};

const PATIENCE: Duration = Duration::from_secs(5);

/// Changes one field of a claim.
type Change = fn(&mut Claim);

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

/// An inhibit for `gesture`, bound to its armed microphone generation.
fn bound(owner: &OutputOwner, gesture: u32) -> (InhibitToken, CollectionGate, Generation) {
    let token = owner.acquire(GestureId(gesture)).expect("acquired");
    let gate = CollectionGate::default();
    let mic = gate.arm(GestureId(gesture)).expect("armed");
    owner.bind_mic(&token, &mic).expect("bound");
    (token, gate, mic)
}

fn retired(owner: &OutputOwner) -> bool {
    owner.view().why == Some(Why::Retired)
}

#[test]
fn a_clean_off_retires_the_stream_keeps_the_session_and_allows_voice_only_capture() {
    let owner = OutputOwner::default();
    let rig = rig(&owner, 1);
    rig.short(480);
    let (token, _gate, _mic) = bound(&owner, 1);
    assert_eq!(
        owner.claim(&token, 1).map(drop),
        Err(Denied::Unqualified),
        "content was exposed"
    );
    rig.script().play_to = u64::MAX;
    until("played out, under the pause", || {
        rig.snapshot()
            .is_some_and(|now| !now.owed() && now.quiet == 1)
    });
    assert_eq!(
        owner.claim(&token, 1).map(drop),
        Err(Denied::Unqualified),
        "quiet, but D1 qualifies no route for content it played"
    );
    assert_eq!(
        owner.view().key,
        Some(OutputKey::Generation(rig.generation)),
        "a live generation that caught up may still speak: not settled"
    );
    owner.withdraw();
    until("retired", || retired(&owner));
    let claim = owner
        .claim(&token, 1)
        .expect("voice-only after a clean off");
    let reserved = owner.start_mic(&claim).expect("starts");
    assert!(
        MicOwner::start_reserved(reserved, Arc::new(AtomicBool::new(true))).is_ok(),
        "the microphone starts"
    );
    let unavailable = ControlToBrain::AudioOutputUnavailable {
        generation: rig.generation,
    };
    assert!(rig.heard(&unavailable), "the brain was told");
    let granted = rig.granted.load(Ordering::SeqCst);
    rig.speak(RunId(2), vec![7; 240]);
    until("stale speech is credited", || {
        rig.granted.load(Ordering::SeqCst) == granted + 3
    });
    assert_eq!(rig.script().content.len(), 480, "nothing more played");
    assert!(
        !rig.ended.load(Ordering::SeqCst) && !rig.aborted.load(Ordering::SeqCst),
        "the session goes on"
    );
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
fn a_teardown_that_plays_past_the_last_content_frame_on_silence_settles_clean() {
    let owner = OutputOwner::default();
    let mut rig = rig(&owner, 1);
    let exposed = rig.short(480);
    owner.end(1);
    let speaker = rig.speaker.take().expect("held");
    std::thread::scope(|scope| {
        let teardown = scope.spawn(move || drop(speaker));
        until("silence released after the link let go", || {
            rig.script().silent > 0
        });
        rig.script().play_to = u64::MAX;
        teardown.join().expect("let go");
    });
    let mut released = None;
    until("released", || {
        released = rig.snapshot().filter(|now| now.flags & RELEASED != 0);
        released.is_some()
    });
    let released = released.expect("read");
    assert_eq!(
        (
            released.exposed,
            released.flags & (FAULT | UNKNOWN),
            released.owed()
        ),
        (exposed, 0, false),
        "played past its last content frame on filler: {released:?}"
    );
    assert_eq!(
        rig.script().content.len(),
        480,
        "no content after the link let go"
    );
    assert!(!owner.view().terminal, "no debt left");
    assert!(
        state(&owner).ledger.iter().all(Option::is_none),
        "its slot is free"
    );
}

#[test]
fn a_stream_that_retires_before_its_open_returns_leaves_the_session_retired() {
    let owner = OutputOwner::default();
    let (generation, tail) = owner.begin(1, Some(Arc::new(|| {}))).expect("reserved");
    let (control, _brain) = async_channel::bounded(64);
    let script = fake::Shared::default();
    let outlet = Outlet {
        generation,
        control,
        grant: Box::new(|_| {}),
        tail: Arc::clone(&tail),
        wake: owner.waker(),
    };
    let speaker = fake::open(outlet, Arc::clone(&script)).expect("ready");
    fake::lock(&script).fail = true;
    until("released before the open returned", || {
        tail.read().is_some_and(|now| now.flags & RELEASED != 0)
    });
    assert_eq!(owner.view().key, None, "still opening");
    let speaker = owner
        .opened(generation, Some(speaker))
        .expect("fenced")
        .expect("kept");
    let key = owner.view().key.expect("a key");
    assert_eq!(
        (key, owner.view().why),
        (OutputKey::NoOutput(1), Some(Why::Retired)),
        "never live without its entry"
    );
    let (token, _gate, _mic) = bound(&owner, 1);
    assert_eq!(
        owner.claim(&token, 1).map(|claim| claim.key),
        Ok(key),
        "voice-only on the session that goes on"
    );
    owner.end(1);
    drop(speaker);
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
    let (token, _gate, _mic) = bound(&owner, 1);
    assert_eq!(
        owner.claim(&token, 1).map(drop),
        Err(Denied::Debt),
        "its thread still holds the tail"
    );
    let changes = owner.changes();
    while changes.try_recv().is_ok() {}
    release.send(()).expect("still held");
    let (woke, woken) = mpsc::channel();
    std::thread::spawn(move || woke.send(changes.recv_blocking().is_ok()));
    assert_eq!(woken.recv_timeout(PATIENCE), Ok(true), "its last wake");
    assert!(
        owner.claim(&token, 1).is_ok(),
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

#[test]
fn retirement_needs_proof_on_the_bound_microphone() {
    let owner = OutputOwner::default();
    assert!(owner.begin(1, None).is_none(), "no opener");
    let (token, _gate, _mic) = bound(&owner, 1);
    let (token, denied) = owner
        .retire(token, Retirement::NoMic)
        .expect_err("a microphone is bound");
    assert_eq!(denied, Denied::Inhibited, "kept");
    let claim = owner.claim(&token, 1).expect("claims");
    let reserved = owner.start_mic(&claim).expect("starts");
    let mut source =
        MicOwner::start_reserved(reserved, Arc::new(AtomicBool::new(true))).expect("spawned");
    let (token, denied) = owner
        .retire(token, Retirement::Unstarted)
        .expect_err("it started");
    assert_eq!(denied, Denied::Inhibited, "kept");
    let (token, denied) = owner
        .retire(token, Retirement::Retired)
        .expect_err("not joined yet");
    assert_eq!(denied, Denied::Inhibited, "kept");
    until("joined", || source.try_receipt().is_some());
    assert!(
        owner.retire(token, Retirement::Retired).is_ok(),
        "the gate shows it retired"
    );
    assert!(owner.acquire(GestureId(2)).is_ok(), "the inhibit is free");
}

#[test]
fn a_claim_matches_every_field() {
    let owner = OutputOwner::default();
    assert!(owner.begin(1, None).is_none(), "no opener");
    let (token, _gate, _mic) = bound(&owner, 1);
    let claim = owner.claim(&token, 1).expect("claims");
    let copy = |claim: &Claim| Claim {
        session: claim.session,
        key: claim.key,
        owner: InhibitToken {
            serial: claim.owner.serial,
            gesture: claim.owner.gesture,
            revision: claim.owner.revision,
        },
    };
    let changed: [(&str, Change); 5] = [
        ("session", |claim| claim.session += 1),
        ("serial", |claim| claim.owner.serial += 1),
        ("revision", |claim| claim.owner.revision += 1),
        ("gesture", |claim| claim.owner.gesture = GestureId(9)),
        ("key", |claim| claim.key = OutputKey::NoOutput(99)),
    ];
    for (field, change) in changed {
        let mut stale = copy(&claim);
        change(&mut stale);
        assert_eq!(
            owner.start_mic(&stale).map(drop),
            Err(Denied::Stale),
            "{field}"
        );
    }
    assert!(owner.start_mic(&claim).is_ok(), "the claim itself starts");
    owner
        .retire(token, Retirement::Retired)
        .expect("let go unstarted, it retired");
    owner.end(1);
    assert!(owner.begin(2, None).is_none(), "no opener");
    let (token, _gate, _mic) = bound(&owner, 2);
    let current = owner.claim(&token, 2).expect("claims");
    assert_ne!(
        current.key, claim.key,
        "each session has its own no-output epoch"
    );
    let mut earlier = copy(&current);
    earlier.key = claim.key;
    assert_eq!(
        owner.start_mic(&earlier).map(drop),
        Err(Denied::Stale),
        "a key from an earlier session"
    );
}

#[test]
fn a_claim_whose_output_moved_on_neither_starts_nor_collects() {
    let alive = Arc::new(AtomicBool::new(true));
    {
        let owner = OutputOwner::default();
        let rig = rig(&owner, 1);
        let (token, _gate, mic) = bound(&owner, 1);
        until("quiet under the pause", || {
            rig.snapshot().is_some_and(|now| now.quiet == 1)
        });
        let claim = owner
            .claim(&token, 1)
            .expect("a paused generation that exposed nothing");
        owner.withdraw();
        until("retired", || retired(&owner));
        assert_eq!(
            owner.start_mic(&claim).map(drop),
            Err(Denied::Stale),
            "the key moved on"
        );
        assert!(mic.discard_unstarted().is_ok(), "it never started");
    }
    let owner = OutputOwner::default();
    let rig = rig(&owner, 1);
    let (token, _gate, mic) = bound(&owner, 1);
    until("quiet under the pause", || {
        rig.snapshot().is_some_and(|now| now.quiet == 1)
    });
    let claim = owner.claim(&token, 1).expect("claims");
    let (release, held) = mpsc::channel::<()>();
    let source = MicOwner::start_reserved_with_for_tests(
        owner.start_mic(&claim).expect("starts"),
        Arc::clone(&alive),
        move |generation, _, running, _| {
            generation.running()?;
            running
                .try_send(Running::for_tests(generation.gesture(), 16_000))
                .map_err(|_| MicError::Unavailable)?;
            let _released = held.recv();
            Ok(Finalization::Discarded(MicError::Cancelled))
        },
    )
    .expect("spawned");
    let running = source.running.recv_blocking().expect("running");
    owner.withdraw();
    until("retired", || retired(&owner));
    assert_eq!(
        owner.collect(&claim, &source, running),
        Err(Denied::Stale),
        "the key moved on"
    );
    assert_eq!(
        mic.mode(&alive, Instant::now()),
        Ok(Mode::Preparing),
        "it never collected"
    );
    release.send(()).expect("still running");
}

/// A capture that says it runs, then waits until `held` lets go.
fn parked(
    held: mpsc::Receiver<()>,
) -> impl FnOnce(
    &Generation,
    &AtomicBool,
    &async_channel::Sender<Running>,
    &async_channel::Sender<MicPcm>,
) -> Result<Finalization, MicError>
+ Send
+ 'static {
    move |generation, _, running, _| {
        generation.running_for_tests()?;
        running
            .try_send(Running::for_tests(generation.gesture(), 16_000))
            .map_err(|_| MicError::Unavailable)?;
        let _released = held.recv();
        Ok(Finalization::Discarded(MicError::Cancelled))
    }
}

#[test]
fn only_the_bound_microphone_collects_under_a_claim() {
    let alive = Arc::new(AtomicBool::new(true));
    let owner = OutputOwner::default();
    assert!(owner.begin(1, None).is_none(), "no opener");
    let (token, _gate, mic) = bound(&owner, 1);
    let claim = owner.claim(&token, 1).expect("claims");
    let other = CollectionGate::default()
        .arm(GestureId(1))
        .expect("another gate, the same gesture number");
    let (release, held) = mpsc::channel::<()>();
    let source = MicOwner::start_reserved_with_for_tests(
        other.reserve().expect("its own start"),
        Arc::clone(&alive),
        parked(held),
    )
    .expect("spawned");
    let running = source.running.recv_blocking().expect("running");
    assert_eq!(
        owner.collect(&claim, &source, running),
        Err(Denied::Stale),
        "not the bound microphone"
    );
    assert_eq!(
        other.mode(&alive, Instant::now()),
        Ok(Mode::Preparing),
        "it never collected"
    );
    let (release_bound, held) = mpsc::channel::<()>();
    let bound_source = MicOwner::start_reserved_with_for_tests(
        owner.start_mic(&claim).expect("starts"),
        Arc::clone(&alive),
        parked(held),
    )
    .expect("spawned");
    let running = bound_source.running.recv_blocking().expect("running");
    assert_eq!(owner.collect(&claim, &bound_source, running), Ok(()));
    assert_eq!(mic.mode(&alive, Instant::now()), Ok(Mode::Collecting));
    for release in [release, release_bound] {
        release.send(()).expect("still running");
    }
}

#[test]
fn a_contended_acquire_is_refused_without_waiting() {
    let owner = Arc::new(OutputOwner::default());
    let held = state(&owner);
    let (done, acquired) = mpsc::channel();
    let contender = Arc::clone(&owner);
    std::thread::spawn(move || done.send(contender.acquire(GestureId(1))));
    let refused = acquired.recv_timeout(Duration::from_secs(1));
    drop(held);
    assert_eq!(refused, Ok(Err(Denied::Busy)), "refused at once");
    assert!(owner.acquire(GestureId(2)).is_ok(), "no inhibit was taken");
}

#[test]
fn a_pause_the_speaker_cannot_take_ends_the_session_and_denies_the_gesture() {
    let owner = OutputOwner::default();
    let rig = rig(&owner, 1);
    rig.script().frozen = true;
    until("parked", || rig.script().parked);
    for run in 1..=16 {
        assert!(owner.stop(1, RunId(run)), "queued");
    }
    assert!(!rig.aborted.load(Ordering::SeqCst), "not yet");
    assert_eq!(
        owner.acquire(GestureId(1)).map(drop),
        Err(Denied::Lost),
        "the pause was refused"
    );
    assert!(
        rig.aborted.load(Ordering::SeqCst),
        "the session's pipes were aborted"
    );
    assert_eq!(owner.view().key, None, "no output");
    rig.script().frozen = false;
}
