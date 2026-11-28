//! An attended pair whose peer scripts one conducted reply's frames, for the link's tests and
//! for the shell's test of its ordinary launch.

use std::cell::Cell;
use std::io::Write as _;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    AudioSession, ControlToBrain, ControlToShell, DataToBrain, DataToShell, Incarnation, Narration,
    OutputGeneration, Phase, RunId, RunOrigin, SceneId, ScreenChoice, SegmentId, SegmentStart,
    SpeechDecision, SpeechDispatch, SpeechPcm, SpeechPromise, StepRef, ThreadId,
};

use super::fixture::{
    Attended, PATIENCE, attended_prepared, attended_with, next, patient, queued_named_run_as,
};
use super::policy::{Offered, STOP_RETRY, SpeakerStops, Told};
use super::{LinkEvent, LinkHandle, LinkStart, Session, Shared};
use crate::presenter::{DrawKey, Effect, Presenter, fixture};
use crate::speaker::conductor::{Assignment, View};
use crate::speaker::queued::{FixturePlayback, serialize_playback};

pub(crate) struct Pair {
    pub(crate) link: Attended,
    events: mpsc::Receiver<LinkEvent>,
    pub(crate) session: Session,
    pub(crate) output: OutputGeneration,
    pub(crate) run: RunId,
    pub(crate) presenter: Presenter,
    woken: Cell<bool>,
    pub(crate) playback: mpsc::Receiver<FixturePlayback>,
    _device: std::sync::MutexGuard<'static, ()>,
}

impl Pair {
    pub(crate) fn new(label: &str) -> Self {
        Self::with(label, AudioSession::QueuedRequired, Narration::Conducted)
    }

    /// A prepared link itself, with its own handle and events, attended against the scripted peer
    /// and a fake device instead of a brain; its talk-back registration must declare Conducted.
    fn attend(
        label: &str,
        (handle, start): (LinkHandle, LinkStart),
        events: async_channel::Receiver<LinkEvent>,
    ) -> Self {
        let serial = serialize_playback();
        let (device, playback) = mpsc::sync_channel(1);
        start
            .link
            .shared
            .fake_playback
            .set(device)
            .expect("the prepared link has no device yet");
        let link = attended_prepared(label, (handle, start), events, Incarnation(1));
        Self::attended(link, serial, playback, Narration::Conducted)
    }

    fn with(label: &str, audio: AudioSession, narration: Narration) -> Self {
        let serial = serialize_playback();
        let (device, playback) = mpsc::sync_channel(1);
        let link = attended_with(
            label,
            Arc::new(Shared {
                narrated: narration == Narration::Conducted,
                fake_playback: device.into(),
                ..Shared::default()
            }),
            Incarnation(1),
            audio,
        );
        Self::attended(link, serial, playback, narration)
    }

    fn attended(
        link: Attended,
        serial: std::sync::MutexGuard<'static, ()>,
        playback: mpsc::Receiver<FixturePlayback>,
        narration: Narration,
    ) -> Self {
        let events = patient(link.events.clone());
        let (session, output) = queued_named_run_as(&link, &events, narration);
        let mut presenter = fixture::presenter();
        presenter.connected(session.generation());
        let effects = presenter
            .described(
                session.generation(),
                &kumochi_scene::text::Bounded::default(),
                Instant::now(),
            )
            .expect("the scripted pair holds");
        let mut pair = Self {
            link,
            events,
            session,
            output,
            run: RunId(7),
            presenter,
            woken: Cell::new(false),
            playback,
            _device: serial,
        };
        pair.settle(effects);
        pair.session.attach_narration(&mut pair.presenter);
        pair
    }

    pub(crate) fn assignment(&self, scene: SceneId, segment: u32) -> Assignment {
        Assignment {
            session: self.session.generation(),
            output: self.output,
            run: self.run,
            segment: SegmentId(segment),
            step: StepRef { scene, index: 0 },
        }
    }

    pub(crate) fn send(&self, message: &DataToShell) {
        let encoded = frame::encode(message, Pipe::Data).expect("the scripted pair holds");
        (&*self.link.data)
            .write_all(&encoded)
            .expect("the scripted pair holds");
    }

    pub(crate) fn reply(&self, message: &ControlToShell) {
        frame::write(&mut &self.link.control, message, Pipe::Control)
            .expect("the scripted pair holds");
    }

    pub(crate) fn settle(&mut self, mut effects: Vec<Effect>) {
        let generation = self.session.generation();
        loop {
            let now = Instant::now();
            while let Some(effect) = effects.pop() {
                let more = match effect {
                    Effect::Admit(job) => self.presenter.completed(job.run(), now),
                    Effect::Prepare(job) => {
                        self.presenter.completed(job.run(&mut fixture::Shaper), now)
                    }
                    Effect::Recover(job) => {
                        self.presenter.completed(job.run(&mut fixture::Shaper), now)
                    }
                    Effect::ConsumptionReady { scene, index } => self
                        .presenter
                        .consumption_ready(generation, scene, index, now),
                    Effect::Credit(_) | Effect::Control(_) => continue,
                };
                effects.extend(more.expect("the scripted pair holds"));
            }
            effects = self
                .presenter
                .advance(generation, now)
                .expect("the scripted pair holds");
            if effects.is_empty() {
                return;
            }
        }
    }

    /// The next event other than a narration wake, which it takes for the next `wake`.
    pub(crate) fn event(&self) -> LinkEvent {
        loop {
            match next(&self.events) {
                LinkEvent::NarrationReady(wake) => {
                    if self.session.accepts_narration(wake) {
                        self.woken.set(true);
                    }
                }
                event => return event,
            }
        }
    }

    /// The next event, which must be the reader delivering `message`.
    pub(crate) fn delivered(&self, message: &DataToShell) -> LinkEvent {
        let event = self.event();
        assert!(
            matches!(&event, LinkEvent::SceneData { frame, .. } if **frame == *message)
                || matches!(&event, LinkEvent::Data { message: seen, .. } if seen == message),
            "the reader delivers {message:?}, got {event:?}"
        );
        event
    }

    /// A scene frame the presenter takes as it arrives.
    pub(crate) fn scene(&mut self, message: &DataToShell) {
        self.send(message);
        let LinkEvent::SceneData { frame, bytes, .. } = self.delivered(message) else {
            unreachable!()
        };
        let effects = self
            .presenter
            .data(self.session.generation(), frame, bytes, Instant::now())
            .expect("the scripted pair holds");
        self.settle(effects);
    }

    pub(crate) fn begin(&mut self, scene: SceneId) {
        self.scene(&DataToShell::SceneBegin {
            scene,
            header: fixture::header(),
        });
    }

    pub(crate) fn commit(a: Assignment) -> DataToShell {
        DataToShell::SceneStepCommit {
            run: a.run,
            scene: a.step.scene,
            index: a.step.index,
            step: fixture::step(
                r#"{"step":{"id":"s","start":"speech","say":"words","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10],"enter":"none"}}]}}"#,
            ),
            speech: Some(SpeechPromise {
                output_generation: a.output,
                segment: a.segment,
            }),
        }
    }

    pub(crate) fn announce(&self, a: Assignment) {
        self.send(&DataToShell::SegmentQueued {
            run: a.run,
            segment: a.segment,
            step: Some(a.step),
            start: SegmentStart::Speech,
            output_generation: a.output,
        });
    }

    /// The commit and announcement of a speech-start step, taken by the presenter.
    pub(crate) fn promise(&mut self, a: Assignment) {
        self.scene(&Self::commit(a));
        self.announce(a);
    }

    /// One final chunk of at most 2,400 samples.
    pub(crate) fn pcm(&self, segment: u32, samples: usize) {
        self.chunk(segment, 0, vec![9; samples], Some(samples));
    }

    /// A line as legal 100 ms chunks, then the empty final that ends its segment.
    pub(crate) fn sound(&self, segment: u32, samples: &[i16]) {
        let total = samples.len();
        let mut chunks: Vec<Vec<i16>> = samples.chunks(2_400).map(<[i16]>::to_vec).collect();
        chunks.push(Vec::new());
        for (seq, chunk) in (0..).zip(chunks) {
            let last = chunk.is_empty().then_some(total);
            self.chunk(segment, seq, chunk, last);
        }
    }

    fn chunk(&self, segment: u32, seq: u32, samples: Vec<i16>, last: Option<usize>) {
        self.send(&DataToShell::SpeechPcm(SpeechPcm {
            run: self.run,
            segment: SegmentId(segment),
            output_generation: self.output,
            seq,
            samples,
            last: last.map(|total| u32::try_from(total).expect("the scripted pair holds")),
        }));
    }

    /// The opened device and the first sound its callback plays.
    pub(crate) fn first_sound(&self) -> (FixturePlayback, f32) {
        let mut device = self
            .playback
            .recv_timeout(PATIENCE)
            .expect("the device opens");
        let deadline = Instant::now() + PATIENCE;
        loop {
            assert!(Instant::now() < deadline, "the callback plays sound");
            let mut out = [0.0; 240];
            device.render(&mut out);
            if let Some(&sample) = out.iter().find(|sample| **sample != 0.0) {
                return (device, sample);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    pub(crate) fn update(&mut self) {
        assert!(self.session.update_narration(
            &self.link.handle,
            &mut self.presenter,
            Instant::now()
        ));
    }

    /// Takes the next narration wake and reconciles it.
    pub(crate) fn wake(&mut self) {
        while !self.woken.replace(false) {
            match next(&self.events) {
                LinkEvent::NarrationReady(wake) => {
                    self.woken.set(self.session.accepts_narration(wake));
                }
                event => panic!("a narration wake, got {event:?}"),
            }
        }
        self.update();
    }

    pub(crate) fn bound(&mut self) -> Option<(Assignment, View)> {
        self.presenter
            .conductor_view(Instant::now())
            .map(|level| (level.binding.assignment, level.view))
    }

    /// Every speech decision the Shell queued before the answer to a fresh Ping.
    pub(crate) fn decisions(&self) -> Vec<ControlToBrain> {
        self.reply(&ControlToShell::Ping);
        let mut decisions = Vec::new();
        loop {
            match self.link.next_control() {
                ControlToBrain::Pong => return decisions,
                decision @ ControlToBrain::SpeechDecision(..) => decisions.push(decision),
                _ => {}
            }
        }
    }

    pub(crate) fn decision(a: Assignment, decision: SpeechDecision) -> ControlToBrain {
        ControlToBrain::SpeechDecision(dispatch(a), decision)
    }

    pub(crate) fn taken(&self, a: Assignment) {
        self.reply(&ControlToShell::SpeechDecisionTaken(dispatch(a)));
    }

    /// The decisions the next passes send once the reader has taken a receipt.
    pub(crate) fn decided(&mut self) -> Vec<ControlToBrain> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            self.update();
            let decisions = self.decisions();
            if !decisions.is_empty() || Instant::now() > deadline {
                return decisions;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Esc on the current run, through the worker's quiet receipt.
    pub(crate) fn esc(&mut self) {
        let effects = self
            .presenter
            .stop(self.run, Instant::now())
            .expect("the scripted pair holds");
        self.settle(effects);
        let mut stops = SpeakerStops::default();
        stops.push((), self.run, Told::Brain);
        while let Some(((), run, _)) = stops.front() {
            let stopped = self
                .link
                .handle
                .output()
                .stop(self.session.generation(), run);
            match stops.offered(stopped, || self.link.handle.audio_session()) {
                Offered::Next => assert_eq!(stopped, Ok(())),
                Offered::Retry { .. } => std::thread::sleep(STOP_RETRY),
                Offered::End(()) => panic!("Esc's stop found the owner busy on every try"),
            }
        }
        loop {
            match self.link.next_control() {
                ControlToBrain::SpeechStopped { run, .. } if run == self.run => return,
                ControlToBrain::SpeechDecision(..) => panic!("Esc sends no decision"),
                _ => {}
            }
        }
    }

    pub(crate) fn run_end(&self) {
        let end = DataToShell::RunEnd { run: self.run };
        self.send(&end);
        self.delivered(&end);
    }

    /// The data frame the Shell wrote, past the credit it returned for metadata.
    pub(crate) fn data(&self) -> Option<DataToBrain> {
        loop {
            match frame::read::<DataToBrain>(&mut &*self.link.data, Pipe::Data)
                .expect("the scripted pair holds")
            {
                Some(DataToBrain::Credit { .. }) => {}
                other => return other,
            }
        }
    }

    /// The pair ended on its own and closed both pipes.
    pub(crate) fn closed(self) {
        assert_eq!(self.data(), None);
        while let Some(control) =
            frame::read::<ControlToBrain>(&mut &self.link.control, Pipe::Control)
                .expect("the scripted pair holds")
        {
            assert!(!matches!(control, ControlToBrain::SpeechDecision(..)));
        }
        assert!(!self.session.open());
        assert!(self.link.link.join().expect("the scripted pair holds"));
    }

    /// The next typed turn, named as the next run.
    pub(crate) fn next_run(&mut self, turn: u32) {
        self.name_run(turn);
        self.named();
    }

    /// Names the next typed turn's run without waiting for the reader to bind it.
    pub(crate) fn name_run(&mut self, turn: u32) {
        let text = DataToBrain::TextSubmit {
            turn,
            thread: Some(ThreadId(1)),
            screen: ScreenChoice::None,
            text: "next".into(),
        };
        assert!(self.link.handle.send_data(&self.session, text.clone()));
        assert_eq!(self.data(), Some(text));
        self.run = RunId(self.run.0 + 1);
        self.reply(&ControlToShell::RunState {
            origin: Some(RunOrigin::Typed { turn }),
            run: self.run,
            thread: ThreadId(1),
            phase: Phase::Thinking,
            error: None,
        });
    }

    /// The reader bound the named run.
    pub(crate) fn named(&self) {
        loop {
            match self.event() {
                LinkEvent::RunState { run, .. } if run == self.run => return,
                LinkEvent::Settings { .. } => {}
                event => panic!("the next run is named, got {event:?}"),
            }
        }
    }

    pub(crate) fn hold_ready(&self) -> bool {
        let deadline = Instant::now() + PATIENCE;
        while !self
            .link
            .handle
            .shared
            .output
            .queued_hold_ready(self.session.generation())
        {
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        true
    }

    /// Paints once and returns the steps whose first paint it accepted.
    pub(crate) fn paint(&mut self) -> Vec<DrawKey> {
        let now = Instant::now();
        self.update();
        let (_, effects) = self
            .presenter
            .sample_at(now)
            .expect("the scripted pair holds");
        self.settle(effects);
        let frames = self.presenter.frames();
        frames.borrow_mut().shown = true;
        assert!(!frames.borrow().paint(|_, _| Ok::<_, ()>(())));
        self.presenter.begin_painted(now);
        self.presenter
            .take_shown(self.session.generation())
            .into_iter()
            .flatten()
            .map(|shown| DrawKey {
                run: shown.run,
                scene: shown.scene,
                index: shown.index,
            })
            .collect()
    }

    pub(crate) fn applied(&self, a: Assignment) -> bool {
        self.session
            .io
            .narrated
            .as_ref()
            .expect("the scripted pair holds")
            .applied(a)
    }
}

pub(crate) fn dispatch(a: Assignment) -> SpeechDispatch {
    SpeechDispatch {
        run: a.run,
        output: a.output,
        segment: a.segment,
    }
}

pub(crate) fn key(a: Assignment) -> DrawKey {
    DrawKey {
        run: a.run,
        scene: a.step.scene,
        index: a.step.index,
    }
}

/// Takes the head from its first binding to its sent Allow.
pub(crate) fn allow(pair: &mut Pair, a: Assignment) {
    let deadline = Instant::now() + PATIENCE;
    let allowed = Pair::decision(a, SpeechDecision::Allow);
    loop {
        assert!(Instant::now() < deadline, "the head's Allow is sent");
        if pair.bound() != Some((a, View::Eligible)) {
            pair.wake();
            continue;
        }
        let decisions = pair.decisions();
        if decisions == [allowed.clone()] {
            return;
        }
        assert!(decisions.is_empty(), "only the head's Allow: {decisions:?}");
        pair.wake();
    }
}

/// Plays a prepared link, with its own handle and event receiver, through two narrated steps of
/// one reply against the scripted peer, the way the ordinary launch would speak them.
///
/// # Panics
/// Panics unless the link registers Conducted on the wire, B's Allow waits for A's exact receipt,
/// each step's own line reaches the device in order with one first paint each, and the exact
/// `RunEnd` admits a fresh speaking Hold.
pub fn conducts_two_steps(
    label: &str,
    prepared: (LinkHandle, LinkStart),
    events: async_channel::Receiver<LinkEvent>,
) {
    const A: i16 = 8_192;
    const B: i16 = 24_576;
    let mut pair = Pair::attend(label, prepared, events);
    let (a, b) = (
        pair.assignment(SceneId(1), 0),
        pair.assignment(SceneId(2), 1),
    );
    pair.begin(a.step.scene);
    pair.begin(b.step.scene);
    pair.promise(a);
    pair.promise(b);
    pair.wake();
    allow(&mut pair, a);
    pair.sound(0, &[A; 4_800]);
    let mut device = pair
        .playback
        .recv_timeout(PATIENCE)
        .expect("A's line opens the device");
    let (mut heard, mut shown) = (Vec::new(), Vec::new());
    let mut play = |pair: &mut Pair, heard: &mut Vec<u32>, shown: &mut Vec<DrawKey>| {
        let mut out = [0.0; 240];
        device.render(&mut out);
        heard.extend(out.iter().filter(|s| **s != 0.0).map(|s| s.to_bits()));
        shown.extend(pair.paint());
        std::thread::sleep(Duration::from_millis(2));
    };
    let deadline = Instant::now() + PATIENCE;
    while pair.bound() != Some((b, View::Eligible)) || !pair.applied(b) {
        assert!(Instant::now() < deadline, "A plays out and B qualifies");
        play(&mut pair, &mut heard, &mut shown);
    }
    assert!(
        pair.decisions().is_empty(),
        "B's decision waits for A's receipt"
    );
    pair.taken(a);
    assert_eq!(
        pair.decided(),
        [Pair::decision(b, SpeechDecision::Allow)],
        "the receipt releases exactly B's Allow"
    );
    pair.taken(b);
    pair.sound(1, &[B; 4_800]);
    let deadline = Instant::now() + PATIENCE;
    while !shown.contains(&key(b)) || heard.last() != Some(&(f32::from(B) / 32_768.0).to_bits()) {
        assert!(Instant::now() < deadline, "B plays and shows");
        play(&mut pair, &mut heard, &mut shown);
    }
    heard.dedup();
    assert_eq!(
        heard,
        [A, B].map(|level| (f32::from(level) / 32_768.0).to_bits()),
        "each step's own line plays, in order"
    );
    assert_eq!(
        shown,
        [key(a), key(b)],
        "each step's first paint, once, in order"
    );
    pair.run_end();
    let deadline = Instant::now() + PATIENCE;
    while !pair
        .link
        .handle
        .shared
        .output
        .queued_hold_ready(pair.session.generation())
    {
        assert!(
            Instant::now() < deadline,
            "the exact RunEnd admits a fresh speaking Hold"
        );
        play(&mut pair, &mut heard, &mut shown);
    }
    assert!(pair.session.open(), "the pair stays connected");
}
